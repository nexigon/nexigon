//! Functionality for multiplexing multiple *channels* over a single connection.
//!
//! The communication protocol is implemented abstractly over some transport layer capable
//! of delivering individual *frames*. Each frame is simply a sequence of bytes.
//! Typically, frames are transmitted over a Websocket as binary messages. From an API
//! user's perspective, data is transmitted over channels in *chunks* of binary data.
//!
//! The protocol takes inspiration from [SSH's channels](https://datatracker.ietf.org/doc/html/rfc4254)
//! using a simple credit mechanism for flow and congestion control.
//!
//! This implementation forms the core of all functionality where real-time communication
//! with a device is required, e.g., for port forwarding and for remote shell access.
//! Therefore, it is somewhat performance critical.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic;
use std::sync::atomic::AtomicU64;
use std::task;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::ready;
use parking_lot::Mutex;
use parking_lot::RwLock;
use pin_project::pin_project;
use thiserror::Error;
use tracing::Level;
use tracing::debug;
use tracing::error;
use tracing::trace;
use tracing::warn;

use self::frames::Frame;
use self::frames::FrameChannelAccept;
use self::frames::FrameChannelAdjust;
use self::frames::FrameChannelClose;
use self::frames::FrameChannelClosed;
use self::frames::FrameChannelData;
use self::frames::FrameChannelReject;
use self::frames::FrameChannelRequest;
use self::frames::FrameHello;
use self::frames::FramePing;
use self::frames::FramePong;
use self::frames::PROTOCOL_MAGIC;
use self::transport::Transport;
use self::transport::TransportError;

mod frames;
pub mod transport;

/// Factor for converting `KiB` to bytes.
const KIB: u64 = 1024;
/// Factor for converting `MiB` to bytes.
const MIB: u64 = KIB * 1024;

/// Maximum frame credits for a channel.
const CHANNEL_MAX_FRAME_CREDIT: u32 = 1024;
/// Maximum byte credits for a channel.
const CHANNEL_MAX_BYTE_CREDIT: u32 = (1024 * MIB) as u32;

/// Channel id used to identify a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChannelId(u64);

impl ChannelId {
    /// Size of the channel id.
    const SIZE: usize = 8;

    /// NULL channel id.
    const NULL: Self = Self(0);

    /// Convert the provided bytes to a channel id.
    fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }

    /// Convert the channel ID to bytes.
    fn to_bytes(self) -> [u8; Self::SIZE] {
        self.0.to_be_bytes()
    }
}

/// Frame exchanged over a connection.
pub type EncodedFrame = Bytes;

/// Transport usable for connections.
pub trait ConnectionTransport: Transport<EncodedFrame, EncodedFrame> + Unpin {}

impl<T: Transport<EncodedFrame, EncodedFrame> + Unpin> ConnectionTransport for T {}

/// A reference to a connection trough which channels can be opened.
#[derive(Debug, Clone)]
pub struct ConnectionRef {
    /// Channel for sending frames.
    frame_tx: mpsc::UnboundedSender<Frame>,
    /// Channel for sending commands to the connection.
    cmd_tx: mpsc::UnboundedSender<ConnectionCmd>,
    /// Shared connection state.
    shared: Arc<ConnectionShared>,
}

impl ConnectionRef {
    /// Check whether the connection is currently closing or has been closed.
    pub fn is_closing(&self) -> bool {
        self.cmd_tx.is_closed() || self.frame_tx.is_closed()
    }

    /// Obtain the estimated round-trip time of the connection.
    pub fn estimate_round_trip_time(&self) -> Option<Duration> {
        *self.shared.smoothened_rtt.read()
    }

    /// Obtain an estimate on the number of frames sent over the connection.
    pub fn estimate_frames_sent(&self) -> u64 {
        self.shared.frames_sent.load(atomic::Ordering::Relaxed)
    }

    /// Obtain an estimate on the number of frames received over the connection.
    pub fn estimate_frames_received(&self) -> u64 {
        self.shared.frames_received.load(atomic::Ordering::Relaxed)
    }

    /// Send a frame over the connection.
    ///
    /// Returns `true` if the frame has been successfully queued for sending.
    fn send_frame(&self, frame: Frame) -> bool {
        self.frame_tx.unbounded_send(frame).is_ok()
    }

    /// Send a connection command.
    ///
    /// Returns `true` if the command has been successfully queued.
    fn send_cmd(&self, cmd: ConnectionCmd) -> bool {
        self.cmd_tx.unbounded_send(cmd).is_ok()
    }

    /// Open a new channel over the connection.
    pub async fn open(&mut self, endpoint: &[u8]) -> Result<Channel, OpenError> {
        // Channel id will be assigned by the connection when processing the command.
        let request = FrameChannelRequest::new(ChannelId::NULL, 128, (16 * KIB) as u32, endpoint);
        let (result_tx, result_rx) = oneshot::channel();
        self.send_cmd(ConnectionCmd::OpenChannel { request, result_tx });
        match result_rx.await {
            Ok(result) => result,
            Err(_) => Err(OpenError::Closed),
        }
    }
}

/// Error opening a channel.
#[derive(Debug, Clone, Error)]
pub enum OpenError {
    /// The connection has been closed.
    #[error("the connection has been closed")]
    Closed,
    /// The request has been rejected.
    #[error("the request to open a channel has been rejected")]
    Rejected(Rejection),
}

/// Channel rejection.
#[derive(Debug, Clone)]
pub struct Rejection {
    /// Frame containing the rejection.
    frame: FrameChannelReject,
}

impl Rejection {
    /// Reason why the channel has been rejected.
    pub fn reason(&self) -> &[u8] {
        self.frame.reason()
    }
}

/// A connection over which multiple channels can be multiplexed.
///
/// **The connection must be polled for events to make any progress!**
#[derive(Debug)]
#[must_use]
pub struct Connection<T> {
    /// Underlying transport layer pinned in memory.
    transport: T,
    /// Indicates that no more frames will arrive over the transport layer.
    exhausted: bool,
    /// Connection has been closed.
    closed: bool,
    /// Channel for receiving frames to send over the transport layer.
    frame_rx: mpsc::UnboundedReceiver<Frame>,
    /// Channel for receiving commands for the connection.
    cmd_rx: mpsc::UnboundedReceiver<ConnectionCmd>,
    /// Id of the next channel.
    next_channel_id: u64,
    /// Pending requests for opening channels.
    pending_requests: HashMap<ChannelId, oneshot::Sender<Result<Channel, OpenError>>>,
    /// Channels opened over the connection.
    channels: HashMap<ChannelId, ChannelHandle>,
    /// Interval for pinging the connection.
    ping_interval: tokio::time::Interval,
    /// Last time a ping was sent.
    last_ping: Option<Instant>,
    /// Smoothened estimated round-trip time.
    smoothened_rtt: Option<Duration>,
    /// Indicates whether a pong has been received.
    pong_received: bool,
    /// Reference to this connection.
    this_ref: ConnectionRef,
}

/// Shared connection state.
#[derive(Debug)]
struct ConnectionShared {
    /// Smoothened estimated round-trip time.
    smoothened_rtt: RwLock<Option<Duration>>,
    /// Frames sent over the connection.
    frames_sent: AtomicU64,
    /// Frames received over the connection.
    frames_received: AtomicU64,
}

impl<T: ConnectionTransport> Connection<T> {
    /// Create a connection from the provided transport.
    pub fn new(transport: T) -> Self {
        let (frame_tx, frame_rx) = mpsc::unbounded();
        let (cmd_tx, cmd_rx) = mpsc::unbounded();
        let _ = frame_tx.unbounded_send(FrameHello::new(&PROTOCOL_MAGIC, b"").into());
        let shared = Arc::new(ConnectionShared {
            smoothened_rtt: RwLock::new(None),
            frames_sent: AtomicU64::new(0),
            frames_received: AtomicU64::new(0),
        });
        let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            transport,
            exhausted: false,
            closed: false,
            frame_rx,
            cmd_rx,
            next_channel_id: 1,
            pending_requests: HashMap::new(),
            channels: HashMap::new(),
            ping_interval,
            last_ping: None,
            smoothened_rtt: None,
            pong_received: true,
            this_ref: ConnectionRef {
                frame_tx,
                cmd_tx,
                shared,
            },
        }
    }

    /// Create a reference for the connection.
    pub fn make_ref(&self) -> ConnectionRef {
        self.this_ref.clone()
    }

    /// Reserve a channel id.
    fn reserve_channel_id(&mut self) -> ChannelId {
        let id = ChannelId(self.next_channel_id);
        self.next_channel_id += 1;
        id
    }

    /// Make a new channel based on the provided ids.
    fn make_channel(&mut self, local_id: ChannelId, remote_id: ChannelId) -> Channel {
        let (channel, handle) = Channel::new(local_id, remote_id, self.this_ref.clone());
        self.channels.insert(local_id, handle);
        channel
    }

    /// Handle a connection command.
    fn handle_cmd(&mut self, cmd: ConnectionCmd) {
        match cmd {
            ConnectionCmd::OpenChannel {
                mut request,
                result_tx,
            } => {
                let local_id = self.reserve_channel_id();
                request.set_sender_id(local_id);
                self.this_ref.send_frame(request.into());
                self.pending_requests.insert(local_id, result_tx);
            }
            ConnectionCmd::AcceptChannel {
                mut accept,
                callback,
            } => {
                let remote_id = accept.receiver_id();
                let local_id = self.reserve_channel_id();
                accept.set_sender_id(local_id);
                self.this_ref.send_frame(accept.into());
                let channel = self.make_channel(local_id, remote_id);
                callback(channel);
            }
        }
    }

    /// Handle a frame.
    #[tracing::instrument(level = Level::TRACE, skip_all)]
    fn handle_frame(&mut self, frame: Frame) -> Result<Option<ConnectionEvent>, ProtocolViolation> {
        Ok(match frame {
            Frame::Hello(frame) => {
                debug!(info = frame.info(), "connection established");
                Some(ConnectionEvent::Connected)
            }
            Frame::Close(_) => {
                debug!("connection closed");
                self.closed = true;
                Some(ConnectionEvent::Closed)
            }
            Frame::ChannelRequest(frame) => {
                debug!(
                    channel.sender_id = frame.sender_id().0,
                    channel.endpoint = frame.endpoint(),
                    "channel requested"
                );
                Some(ConnectionEvent::RequestChannel(ChannelRequest::new(
                    frame,
                    self.make_ref(),
                )))
            }
            Frame::ChannelAccept(frame) => {
                let local_id = frame.receiver_id();
                let remote_id = frame.sender_id();
                debug!(
                    channel.local_id = local_id.0,
                    channel.remote_id = remote_id.0,
                    "channel accepted"
                );
                let channel = self.make_channel(local_id, remote_id);
                let Some(result_tx) = self.pending_requests.remove(&local_id) else {
                    error!("protocol violation: channel request not found");
                    return Err(ProtocolViolation("channel request not found"));
                };
                let _ = result_tx.send(Ok(channel));
                None
            }
            Frame::ChannelReject(frame) => {
                let local_id = frame.receiver_id();
                debug!(
                    channel.local_id = local_id.0,
                    reason = frame.reason(),
                    "channel accepted"
                );
                let Some(result_tx) = self.pending_requests.remove(&local_id) else {
                    error!("protocol violation: channel request not found");
                    return Err(ProtocolViolation("channel request not found"));
                };
                let _ = result_tx.send(Err(OpenError::Rejected(Rejection { frame })));
                None
            }
            Frame::ChannelData(frame) => {
                let local_id = frame.receiver_id();
                trace!(channel.local_id = local_id.0, "received data");
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.receiver_shared.lock();
                    if shared.remaining_frame_credit == 0 {
                        return Err(ProtocolViolation("no frame credit remaining"));
                    }
                    if shared.remaining_byte_credit < frame.payload().len() as u32 {
                        return Err(ProtocolViolation("not enough byte credit"));
                    }
                    shared.buffer.push_back(frame);
                    if let Some(waker) = shared.waker.take() {
                        waker.wake();
                    }
                };
                None
            }
            Frame::ChannelAdjust(frame) => {
                let local_id = frame.receiver_id();
                trace!(channel.local_id = local_id.0, "adjust channel credits");
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.sender_shared.lock();
                    shared.remaining_frame_credit += frame.frame_credit();
                    shared.remaining_byte_credit += frame.byte_credit();
                    let duration = shared.last_credit_update.elapsed().as_secs_f64();
                    let used_byte_credit = shared.used_byte_credit;
                    shared
                        .bandwidth_bytes
                        .update((used_byte_credit as f64) / duration);
                    let used_frame_credit = shared.used_frame_credit;
                    shared
                        .bandwidth_frames
                        .update((used_frame_credit as f64) / duration);
                    shared.used_byte_credit = 0;
                    shared.used_frame_credit = 0;
                    shared.last_credit_update = Instant::now();
                    if let Some(waker) = shared.waker.take() {
                        waker.wake();
                    }
                }
                None
            }
            Frame::ChannelClose(frame) => {
                let local_id = frame.receiver_id();
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.sender_shared.lock();
                    shared.closed = Some(frame);
                    if let Some(waker) = shared.waker.take() {
                        waker.wake();
                    }
                }
                None
            }
            Frame::ChannelClosed(frame) => {
                let local_id = frame.receiver_id();
                debug!(
                    channel.local_id = local_id.0,
                    reason = frame.reason(),
                    "channel closed"
                );
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.receiver_shared.lock();
                    shared.closed = true;
                    if let Some(waker) = shared.waker.take() {
                        waker.wake();
                    }
                }
                None
            }
            Frame::Ping(_) => {
                self.this_ref
                    .frame_tx
                    .unbounded_send(FramePong::new().into())
                    .ok();
                None
            }
            Frame::Pong(_) => {
                self.handle_pong()?;
                None
            }
        })
    }

    /// Send a ping, if necessary.
    fn ping(&mut self, cx: &mut task::Context<'_>) {
        if !self.pong_received {
            // We are still waiting for the pong.
            return;
        }
        if self.ping_interval.poll_tick(cx).is_ready() {
            self.ping_interval.reset();
            self.pong_received = false;
            self.last_ping = Some(Instant::now());
            self.this_ref
                .frame_tx
                .unbounded_send(FramePing::new().into())
                .ok();
        }
    }

    /// Handle a pong.
    fn handle_pong(&mut self) -> Result<(), ProtocolViolation> {
        let Some(last_ping) = self.last_ping else {
            return Err(ProtocolViolation("received pong but no ping has been sent"));
        };
        self.pong_received = true;
        let latest_rtt = last_ping.elapsed();
        if let Some(smoothened_rtt) = self.smoothened_rtt {
            self.smoothened_rtt = Some(smoothened_rtt * 7 / 8 + latest_rtt / 8);
        } else {
            self.smoothened_rtt = Some(latest_rtt);
        }
        *self.this_ref.shared.smoothened_rtt.write() = self.smoothened_rtt;
        Ok(())
    }

    /// Poll the connection for events.
    fn poll_event(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<ConnectionEvent>, ConnectionError<T>>> {
        self.ping(cx);
        loop {
            match self.cmd_rx.poll_next_unpin(cx) {
                Poll::Ready(Some(cmd)) => {
                    self.handle_cmd(cmd);
                }
                Poll::Ready(None) => unreachable!("the connection holds on to a sender"),
                Poll::Pending => break,
            }
        }
        loop {
            match self.transport.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => match self.frame_rx.poll_next_unpin(cx) {
                    Poll::Ready(Some(frame)) => {
                        self.this_ref
                            .shared
                            .frames_sent
                            .fetch_add(1, atomic::Ordering::Relaxed);
                        if let Err(error) = self.transport.start_send_unpin(frame.into()) {
                            return Poll::Ready(Err(ConnectionError::TransportError(
                                TransportError::SendError(error),
                            )));
                        }
                    }
                    Poll::Ready(None) => unreachable!("the connection holds on to a sender"),
                    Poll::Pending => break,
                },
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(ConnectionError::TransportError(
                        TransportError::SendError(error),
                    )));
                }
                Poll::Pending => break,
            }
        }
        if let Poll::Ready(Err(error)) = self.transport.poll_flush_unpin(cx) {
            return Poll::Ready(Err(ConnectionError::TransportError(
                TransportError::SendError(error),
            )));
        }
        while !self.exhausted {
            match self.transport.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(frame))) => match Frame::parse(frame) {
                    Ok(frame) => {
                        self.this_ref
                            .shared
                            .frames_received
                            .fetch_add(1, atomic::Ordering::Relaxed);
                        if let Some(event) = self.handle_frame(frame)? {
                            return Poll::Ready(Ok(Some(event)));
                        }
                    }
                    Err(error) => {
                        error!("received invalid frame: {error}");
                        return Poll::Ready(Err(ConnectionError::ProtocolViolation(
                            ProtocolViolation("invalid frame"),
                        )));
                    }
                },
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(ConnectionError::TransportError(
                        TransportError::RecvError(error),
                    )));
                }
                Poll::Ready(None) => {
                    self.exhausted = true;
                }
                Poll::Pending => {
                    break;
                }
            }
        }
        Poll::Pending
    }
}

impl<T: ConnectionTransport> Stream for Connection<T> {
    type Item = Result<ConnectionEvent, ConnectionError<T>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_event(cx).map(|v| v.transpose())
    }
}

/// Connection error.
#[derive(Debug, Error)]
pub enum ConnectionError<T: ConnectionTransport> {
    /// Error on the transport layer.
    #[error(transparent)]
    TransportError(#[from] TransportError<T::RecvError, T::SendError>),
    /// Protocol violation.
    #[error(transparent)]
    ProtocolViolation(#[from] ProtocolViolation),
}

/// Protocol violation.
#[derive(Debug, Error)]
#[error("protocol violation: {0}")]
pub struct ProtocolViolation(&'static str);

/// Connection event.
#[derive(Debug)]
pub enum ConnectionEvent {
    /// Connection has been initialized.
    Connected,
    /// Connection should be closed.
    Closed,
    /// Request a channel to be opened.
    RequestChannel(ChannelRequest),
}

/// A command to control the connection.
enum ConnectionCmd {
    /// Open a new channel.
    OpenChannel {
        request: FrameChannelRequest,
        result_tx: oneshot::Sender<Result<Channel, OpenError>>,
    },
    /// Accept a channel request.
    AcceptChannel {
        accept: FrameChannelAccept,
        callback: Box<dyn Send + FnOnce(Channel)>,
    },
}

/// Request to open a channel.
#[must_use]
#[derive(Debug)]
pub struct ChannelRequest {
    /// Frame.
    request: FrameChannelRequest,
    /// Connection.
    connection: ConnectionRef,
    /// Indicates whether the request has been handled.
    handled: bool,
}

impl ChannelRequest {
    /// Create a new channel request.
    fn new(request: FrameChannelRequest, connection: ConnectionRef) -> Self {
        Self {
            request,
            connection,
            handled: false,
        }
    }

    /// Endpoint of the request.
    pub fn endpoint(&self) -> &[u8] {
        self.request.endpoint()
    }

    /// Reject the request.
    fn mut_reject(&mut self, reason: &[u8]) {
        assert!(!self.handled, "request must not be rejected twice");
        self.handled = true;
        let reject = FrameChannelReject::new(self.request.sender_id(), reason);
        self.connection.send_frame(reject.into());
    }

    /// Reject the request.
    pub fn reject(mut self, reason: &[u8]) {
        self.mut_reject(reason);
    }

    /// Accept the request.
    ///
    /// When the channel has been accepted, the provided callback is called with the
    /// channel.
    pub fn accept(mut self, callback: impl 'static + Send + FnOnce(Channel)) {
        self.handled = true;
        let accept = FrameChannelAccept::new(
            self.request.sender_id(),
            ChannelId::NULL,
            128,
            (16 * KIB) as u32,
        );
        self.connection.send_cmd(ConnectionCmd::AcceptChannel {
            accept,
            callback: Box::new(callback),
        });
    }
}

impl Drop for ChannelRequest {
    fn drop(&mut self) {
        if !self.handled {
            warn!("channel request has been dropped without being handled");
            self.mut_reject(b"");
        }
    }
}

/// Channel handle to be stored in the connection.
#[derive(Debug)]
struct ChannelHandle {
    /// Local id of the channel.
    #[expect(dead_code, reason = "unused")]
    local_id: ChannelId,
    /// Remote id of the channel.
    #[expect(dead_code, reason = "unused")]
    remote_id: ChannelId,
    /// Shared sender state.
    sender_shared: Arc<Mutex<SenderShared>>,
    /// Shared receiver state.
    receiver_shared: Arc<Mutex<ReceiverShared>>,
}

/// Bi-directional channel.
#[pin_project]
pub struct Channel {
    /// Sender.
    #[pin]
    sender: Sender,
    /// Receiver.
    #[pin]
    receiver: Receiver,
}

impl Channel {
    /// Create a new channel on the given connection with the given ids.
    fn new(
        local_id: ChannelId,
        remote_id: ChannelId,
        connection: ConnectionRef,
    ) -> (Self, ChannelHandle) {
        let channel = Self {
            sender: Sender {
                shared: Arc::new(Mutex::new(SenderShared::new(128, (16 * KIB) as u32))),
                remote_id,
                connection: connection.clone(),
                pending: None,
            },
            receiver: Receiver {
                shared: Arc::new(Mutex::new(ReceiverShared::new())),
                remote_id,
                connection,
                pending: None,
                offset: 0,
            },
        };
        let handle = ChannelHandle {
            local_id,
            remote_id,
            receiver_shared: channel.receiver.shared.clone(),
            sender_shared: channel.sender.shared.clone(),
        };
        (channel, handle)
    }

    /// Merge the sender and receiver into a single channel.
    ///
    /// # Panics
    ///
    /// Panics in case the sender and receiver do not belong to the same channel.
    pub fn merge(sender: Sender, receiver: Receiver) -> Self {
        assert!(
            Arc::ptr_eq(&sender.connection.shared, &receiver.connection.shared),
            "sender and receiver belong to different connections"
        );
        assert!(
            sender.remote_id == receiver.remote_id,
            "sender and receiver belong to different channels"
        );
        Self { sender, receiver }
    }

    /// Split the channel into sender and receiver.
    pub fn split(self) -> (Sender, Receiver) {
        (self.sender, self.receiver)
    }
}

impl AsyncWrite for Channel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.project().sender.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().sender.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().sender.poll_close(cx)
    }
}

impl tokio::io::AsyncWrite for Channel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_close(self, cx)
    }
}

impl AsyncRead for Channel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.project().receiver.poll_read(cx, buf)
    }
}

impl tokio::io::AsyncRead for Channel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        tokio::io::AsyncRead::poll_read(self.project().receiver, cx, buf)
    }
}

/// Factor used to smoothing the bandwidth computations.
const BANDWIDTH_SMOOTHENING_FACTOR: f64 = 0.5;

/// Auxiliary macro for polling fallible futures.
macro_rules! try_ready {
    ($value:expr) => {
        match $value {
            Poll::Ready(Ok(value)) => value,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }
    };
}

/// Chunk of received or to be send over a channel.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Underlying data frame.
    frame: FrameChannelData<Bytes>,
}

impl Chunk {
    /// Construct a new chunk with the given capacity.
    fn with_capacity(capacity: usize) -> Self {
        let mut bytes =
            BytesMut::with_capacity(FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE + capacity);
        bytes.put_u8(FrameChannelData::<Vec<u8>>::FRAME_TAG);
        bytes.extend(ChannelId::NULL.to_bytes());
        Self {
            frame: FrameChannelData::from_raw_bytes(bytes.freeze()),
        }
    }

    /// Extend the chunk with the given bytes.
    fn extend(&mut self, bytes: &[u8]) {
        let mut frame_bytes = BytesMut::from(std::mem::take(&mut self.frame.bytes));
        frame_bytes.extend(bytes);
        self.frame.bytes = frame_bytes.freeze();
    }

    /// Extract the chunk data as [`Bytes`].
    ///
    /// This is typically a `O(1)` operation.
    pub fn to_bytes(&self) -> Bytes {
        self.frame
            .bytes
            .clone()
            .split_off(FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE)
    }
}

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.frame.payload()
    }
}

impl From<Chunk> for Bytes {
    fn from(value: Chunk) -> Self {
        value.to_bytes()
    }
}

/// Shared state of the sending end of a channel.
#[derive(Debug)]
struct SenderShared {
    /// Indicates whether the channel has been closed by the receiver.
    closed: Option<FrameChannelClose>,
    /// Optional waker to wake up the sender when something changed.
    waker: Option<Waker>,
    /// Remaining frame credit.
    remaining_frame_credit: u32,
    /// Remaining byte credit.
    remaining_byte_credit: u32,
    /// Time of last credit update.
    last_credit_update: Instant,
    /// Used frame credit since last credit update.
    used_frame_credit: u32,
    /// Used byte credit since last credit update.
    used_byte_credit: u32,
    /// Estimation of the used bandwidth in bytes per second.
    bandwidth_bytes: Ema,
    /// Estimation of the used bandwidth in frames per second.
    bandwidth_frames: Ema,
}

impl SenderShared {
    /// Create a new shared sender state.
    fn new(initial_frame_credit: u32, initial_byte_credit: u32) -> Self {
        Self {
            closed: None,
            waker: None,
            remaining_frame_credit: initial_frame_credit,
            remaining_byte_credit: initial_byte_credit,
            last_credit_update: Instant::now(),
            used_frame_credit: 0,
            used_byte_credit: 0,
            bandwidth_bytes: Ema::new(BANDWIDTH_SMOOTHENING_FACTOR),
            bandwidth_frames: Ema::new(BANDWIDTH_SMOOTHENING_FACTOR),
        }
    }
}

/// Error sending a chunk over a channel.
#[derive(Debug)]
pub enum ChannelSendError {
    /// Chunk is too large.
    ChunkTooLarge,
    /// Channel has been closed.
    Closed,
}

/// Sending end of a channel.
///
/// Implements [`AsyncWrite`] to send data over the channel.
#[derive(Debug)]
pub struct Sender {
    /// Remote id of the channel.
    remote_id: ChannelId,
    /// Shared sender state.
    shared: Arc<Mutex<SenderShared>>,
    /// Connection.
    connection: ConnectionRef,
    /// Pending chunk.
    pending: Option<Chunk>,
}

impl Sender {
    /// Estimated currently used bandwidth in bytes per second.
    pub fn used_bandwidth_bytes(&self) -> f64 {
        self.shared
            .lock()
            .bandwidth_bytes
            .value()
            .unwrap_or_default()
    }

    /// Estimated currently used bandwidth in frames per second.
    pub fn used_bandwidth_frames(&self) -> f64 {
        self.shared
            .lock()
            .bandwidth_frames
            .value()
            .unwrap_or_default()
    }

    /// Send the current chunk, if any.
    fn poll_send_chunk(&mut self, cx: &mut task::Context) -> Poll<Result<(), ChannelSendError>> {
        let mut shared = self.shared.lock();
        if shared.closed.is_some() {
            return Poll::Ready(Err(ChannelSendError::Closed));
        }
        let Some(chunk) = &self.pending else {
            return Poll::Ready(Ok(()));
        };
        let byte_credit = chunk.frame.payload().len() as u32;
        assert!(shared.remaining_byte_credit >= byte_credit);
        if shared.remaining_frame_credit > 0 {
            shared.remaining_frame_credit -= 1;
            shared.remaining_byte_credit -= byte_credit;
            shared.used_frame_credit += 1;
            shared.used_byte_credit += byte_credit;
            let mut frame = self.pending.take().unwrap().frame;
            frame.set_receiver_id(self.remote_id);
            self.connection.send_frame(frame.into());
            Poll::Ready(Ok(()))
        } else {
            shared.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncWrite for Sender {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        try_ready!(AsyncWrite::poll_flush(self.as_mut(), cx));
        let mut shared = self.shared.lock();
        if shared.remaining_byte_credit < 512 {
            shared.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let chunk_size = (shared.remaining_byte_credit as usize).min(buf.len());
        let mut chunk = Chunk::with_capacity(chunk_size);
        chunk.extend(&buf[..chunk_size]);
        drop(shared);
        self.pending = Some(chunk);
        Poll::Ready(Ok(chunk_size))
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match ready!(self.poll_send_chunk(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection has been closed",
            ))),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        try_ready!(self.as_mut().poll_flush(cx));
        self.connection
            .send_frame(FrameChannelClosed::new(self.remote_id, b"").into());
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for Sender {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_close(self, cx)
    }
}

/// Shared state of the receiving end of a channel.
#[derive(Debug)]
struct ReceiverShared {
    /// Buffered frames.
    buffer: VecDeque<FrameChannelData>,
    /// Indicates whether the channel has been closed.
    closed: bool,
    /// Optional waker to wake up the receiver when something changed.
    waker: Option<Waker>,
    /// Maximum frame credit.
    max_frame_credit: u32,
    /// Maximum byte credit.
    max_byte_credit: u32,
    /// Remaining frame credit.
    remaining_frame_credit: u32,
    /// Remaining byte credit.
    remaining_byte_credit: u32,
    /// Last time a credit update was sent.
    last_credit_update: Instant,
    /// Estimation of the used bandwidth in bytes per second.
    bandwidth_bytes: Ema,
    /// Estimation of the used bandwidth in frames per second.
    bandwidth_frames: Ema,
}

impl ReceiverShared {
    /// Create a new receiver shared state.
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            closed: false,
            waker: None,
            max_frame_credit: 128,
            max_byte_credit: (16 * KIB) as u32,
            remaining_frame_credit: 128,
            remaining_byte_credit: (16 * KIB) as u32,
            last_credit_update: Instant::now(),
            bandwidth_bytes: Ema::new(BANDWIDTH_SMOOTHENING_FACTOR),
            bandwidth_frames: Ema::new(BANDWIDTH_SMOOTHENING_FACTOR),
        }
    }
}

/// Receiving end of a channel.
///
/// Implements [`AsyncRead``] to read data from the channel.
#[derive(Debug)]
pub struct Receiver {
    /// Remote id of the channel.
    remote_id: ChannelId,
    /// Shared receiver state.
    shared: Arc<Mutex<ReceiverShared>>,
    /// Connection.
    connection: ConnectionRef,
    /// Pending chunk.
    pending: Option<Bytes>,
    /// Offset into the pending chunk.
    offset: usize,
}

impl Receiver {
    /// Estimated currently used bandwidth in bytes per second.
    pub fn used_bandwidth_bytes(&self) -> f64 {
        self.shared
            .lock()
            .bandwidth_bytes
            .value()
            .unwrap_or_default()
    }

    /// Estimated currently used bandwidth in frames per second.
    pub fn bandwidth_frames(&self) -> f64 {
        self.shared
            .lock()
            .bandwidth_frames
            .value()
            .unwrap_or_default()
    }

    /// Poll the next chunk.
    fn poll_next_chunk(&mut self, cx: &mut task::Context) -> Poll<Option<Chunk>> {
        let mut shared = self.shared.lock();
        if shared.closed {
            return Poll::Ready(None);
        }
        if let Some(frame) = shared.buffer.pop_front() {
            shared.remaining_frame_credit -= 1;
            shared.remaining_byte_credit -= frame.payload().len() as u32;
            let mut update_credit = false;
            let smoothened_rtt = *self.connection.shared.smoothened_rtt.read();
            if shared.remaining_frame_credit < shared.max_frame_credit / 2 {
                if let Some(smoothened_rtt) = smoothened_rtt {
                    if shared.last_credit_update.elapsed() < 2 * smoothened_rtt {
                        shared.max_frame_credit =
                            (shared.max_frame_credit * 2).min(CHANNEL_MAX_FRAME_CREDIT);
                    }
                }
                update_credit = true;
            }
            if shared.remaining_byte_credit < shared.max_byte_credit / 2 {
                if let Some(smoothened_rtt) = smoothened_rtt {
                    if shared.last_credit_update.elapsed() < 2 * smoothened_rtt {
                        shared.max_byte_credit =
                            (shared.max_byte_credit * 2).min(CHANNEL_MAX_BYTE_CREDIT);
                    }
                }
                update_credit = true;
            }
            if update_credit {
                let add_frame_credit = shared.max_frame_credit - shared.remaining_frame_credit;
                let add_byte_credit = shared.max_byte_credit - shared.remaining_byte_credit;
                let duration = shared.last_credit_update.elapsed().as_secs_f64();
                shared
                    .bandwidth_bytes
                    .update((add_byte_credit as f64) / duration);
                shared
                    .bandwidth_frames
                    .update((add_frame_credit as f64) / duration);
                self.connection.send_frame(
                    FrameChannelAdjust::new(self.remote_id, add_frame_credit, add_byte_credit)
                        .into(),
                );
                shared.last_credit_update = Instant::now();
                shared.remaining_frame_credit = shared.max_frame_credit;
                shared.remaining_byte_credit = shared.max_byte_credit;
            }
            return Poll::Ready(Some(Chunk { frame }));
        }
        shared.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.connection
            .send_frame(FrameChannelClose::new(self.remote_id, b"").into());
    }
}

impl AsyncRead for Receiver {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            if let Some(pending) = &self.pending {
                let bytes = (pending.len() - self.offset).min(buf.len());
                buf[..bytes].copy_from_slice(&pending[self.offset..self.offset + bytes]);
                let pending_len = pending.len();
                self.offset += bytes;
                if self.offset >= pending_len {
                    self.pending = None;
                }
                return Poll::Ready(Ok(bytes));
            }
            if let Some(chunk) = ready!(self.as_mut().poll_next(cx)) {
                self.pending = Some(chunk.frame.bytes);
                self.offset = FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE;
            } else {
                return Poll::Pending;
            }
        }
    }
}

impl tokio::io::AsyncRead for Receiver {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if let Some(pending) = &self.pending {
                let bytes = (pending.len() - self.offset).min(buf.remaining());
                buf.put_slice(&pending[self.offset..self.offset + bytes]);
                let pending_len = pending.len();
                self.offset += bytes;
                if self.offset >= pending_len {
                    self.pending = None;
                }
                return Poll::Ready(Ok(()));
            }
            if let Some(chunk) = ready!(self.as_mut().poll_next(cx)) {
                self.pending = Some(chunk.frame.bytes);
                self.offset = FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE;
            } else {
                return Poll::Pending;
            }
        }
    }
}

impl Stream for Receiver {
    type Item = Chunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Option<Self::Item>> {
        self.poll_next_chunk(cx)
    }
}

/// Exponential moving average filter used for RTT and bandwidth computations.
#[derive(Debug)]
pub struct Ema {
    /// Current value.
    value: Option<f64>,
    /// Smoothening factor.
    factor: f64,
}

impl Ema {
    /// Create a new exponential moving average filter.
    pub fn new(factor: f64) -> Self {
        Self {
            value: None,
            factor,
        }
    }

    /// Obtain the current value.
    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// Update the filter with the given value.
    pub fn update(&mut self, value: f64) {
        match self.value {
            Some(last_value) => {
                self.value = Some(value * self.factor + last_value * (1.0 - self.factor));
            }
            None => {
                self.value = Some(value);
            }
        }
    }
}
