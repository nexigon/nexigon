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
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
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
use futures::channel::oneshot;
use futures::ready;
use futures::task::AtomicWaker;
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
const CHANNEL_MAX_BYTE_CREDIT: u32 = MIB as u32;
/// Initial frame credits advertised for a channel.
const CHANNEL_INITIAL_FRAME_CREDIT: u32 = 128;
/// Initial byte credits advertised for a channel.
const CHANNEL_INITIAL_BYTE_CREDIT: u32 = (16 * KIB) as u32;

/// Resource limits for a multiplex connection.
///
/// The initial channel credits are fixed by the legacy protocol implementation. The
/// configurable receive-credit ceilings therefore apply only to subsequent window growth.
/// Requests beyond the channel and pending-request limits are rejected. Exceeding a hard
/// queue, frame-size, or rate limit terminates the connection instead of dropping
/// protocol frames.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// Maximum time allowed for the peer's initial Hello frame.
    pub handshake_timeout: Duration,
    /// Interval between connection liveness probes after the handshake.
    pub ping_interval: Duration,
    /// Maximum time for a queued Ping to be flushed to the transport.
    pub ping_write_timeout: Duration,
    /// Maximum time to wait for a Pong after sending a Ping.
    pub pong_timeout: Duration,
    /// Maximum number of simultaneously open channels.
    pub max_channels: usize,
    /// Maximum number of channel requests waiting for a response.
    pub max_pending_channel_requests: usize,
    /// Maximum number of commands queued by connection references.
    pub max_queued_commands: usize,
    /// Maximum number of frames waiting to be written to the transport.
    pub max_queued_frames: usize,
    /// Maximum encoded size of all frames waiting to be written to the transport.
    pub max_queued_frame_bytes: usize,
    /// Outgoing queue slots unavailable to data frames and reserved for control traffic.
    pub reserved_control_queue_frames: usize,
    /// Outgoing queue bytes unavailable to data frames and reserved for control traffic.
    pub reserved_control_queue_bytes: usize,
    /// Maximum size of one encoded frame.
    pub max_encoded_frame_size: usize,
    /// Maximum receive window for frames on one channel.
    pub max_receive_frame_credit: u32,
    /// Maximum receive window for bytes on one channel.
    pub max_receive_byte_credit: u32,
    /// Maximum number of multiplex ping frames accepted per second.
    pub max_peer_pings_per_second: u32,
    /// Maximum number of channel requests accepted per second.
    pub max_channel_requests_per_second: u32,
    /// Maximum number of non-data protocol frames accepted per second.
    pub max_control_frames_per_second: u32,
    /// Maximum variable-sized payload carried by a control frame.
    pub max_control_payload_size: usize,
    /// Maximum number of frames for unknown channels accepted per second.
    pub max_unknown_channel_frames_per_second: u32,
    /// Maximum number of recently closed channel IDs retained for late-frame handling.
    pub max_closed_channel_tombstones: usize,
    /// Maximum number of incoming frames processed in one poll.
    pub max_frames_processed_per_poll: usize,
}

impl ConnectionLimits {
    /// Validate the limits.
    fn validate(self) {
        let minimum_frame_size =
            FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE + CHANNEL_INITIAL_BYTE_CREDIT as usize;
        assert!(
            !self.handshake_timeout.is_zero(),
            "handshake_timeout must not be zero"
        );
        assert!(
            !self.ping_interval.is_zero(),
            "ping_interval must not be zero"
        );
        assert!(
            !self.ping_write_timeout.is_zero(),
            "ping_write_timeout must not be zero"
        );
        assert!(
            !self.pong_timeout.is_zero(),
            "pong_timeout must not be zero"
        );
        assert!(self.max_channels > 0, "max_channels must not be zero");
        assert!(
            self.max_pending_channel_requests > 0,
            "max_pending_channel_requests must not be zero"
        );
        assert!(
            self.max_queued_commands > 0,
            "max_queued_commands must not be zero"
        );
        assert!(
            self.max_queued_frames > 0,
            "max_queued_frames must not be zero"
        );
        assert!(
            self.reserved_control_queue_frames < self.max_queued_frames,
            "reserved_control_queue_frames must leave room for channel data"
        );
        assert!(
            self.max_encoded_frame_size >= minimum_frame_size,
            "max_encoded_frame_size must accommodate the legacy initial window"
        );
        assert!(
            self.max_queued_frame_bytes >= self.max_encoded_frame_size,
            "max_queued_frame_bytes must accommodate one maximum-sized frame"
        );
        assert!(
            self.reserved_control_queue_bytes < self.max_queued_frame_bytes,
            "reserved_control_queue_bytes must leave room for channel data"
        );
        assert!(
            self.max_queued_frame_bytes - self.reserved_control_queue_bytes
                >= self.max_encoded_frame_size,
            "the data portion of the outgoing queue must accommodate one maximum-sized frame"
        );
        assert!(
            self.max_receive_frame_credit >= CHANNEL_INITIAL_FRAME_CREDIT,
            "max_receive_frame_credit must preserve the legacy initial window"
        );
        assert!(
            self.max_receive_byte_credit >= CHANNEL_INITIAL_BYTE_CREDIT,
            "max_receive_byte_credit must preserve the legacy initial window"
        );
        assert!(
            self.max_peer_pings_per_second > 0,
            "max_peer_pings_per_second must not be zero"
        );
        assert!(
            self.max_channel_requests_per_second > 0,
            "max_channel_requests_per_second must not be zero"
        );
        assert!(
            self.max_control_frames_per_second > 0,
            "max_control_frames_per_second must not be zero"
        );
        assert!(
            self.max_unknown_channel_frames_per_second > 0,
            "max_unknown_channel_frames_per_second must not be zero"
        );
        assert!(
            self.max_closed_channel_tombstones > 0,
            "max_closed_channel_tombstones must not be zero"
        );
        assert!(
            self.max_frames_processed_per_poll > 0,
            "max_frames_processed_per_poll must not be zero"
        );
        assert!(
            self.max_control_payload_size <= self.max_encoded_frame_size,
            "max_control_payload_size must not exceed max_encoded_frame_size"
        );
    }

    /// Maximum payload size emitted in one channel data frame.
    fn max_channel_payload_size(self) -> usize {
        self.max_encoded_frame_size - FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE
    }
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(5),
            ping_write_timeout: Duration::from_secs(30),
            pong_timeout: Duration::from_secs(15),
            max_channels: 32,
            max_pending_channel_requests: 16,
            max_queued_commands: 256,
            max_queued_frames: 1024,
            max_queued_frame_bytes: (8 * MIB) as usize,
            reserved_control_queue_frames: 64,
            reserved_control_queue_bytes: (512 * KIB) as usize,
            max_encoded_frame_size: MIB as usize + FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE,
            max_receive_frame_credit: CHANNEL_MAX_FRAME_CREDIT,
            max_receive_byte_credit: CHANNEL_MAX_BYTE_CREDIT,
            max_peer_pings_per_second: 8,
            max_channel_requests_per_second: 64,
            max_control_frames_per_second: 8192,
            max_control_payload_size: 4 * KIB as usize,
            max_unknown_channel_frames_per_second: 128,
            max_closed_channel_tombstones: 256,
            max_frames_processed_per_poll: 256,
        }
    }
}

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
    /// Shared connection state.
    shared: Arc<ConnectionShared>,
}

impl ConnectionRef {
    /// Check whether the connection is currently closing or has been closed.
    pub fn is_closing(&self) -> bool {
        self.shared.closed.load(atomic::Ordering::Acquire)
            || self.shared.overloaded.load(atomic::Ordering::Acquire)
    }

    /// Terminate the connection locally.
    ///
    /// This is intentionally a local lifecycle operation rather than a new wire-level
    /// message. The connection task is woken, drops its transport, and closes every
    /// channel. Existing peers therefore observe the same transport closure they already
    /// understand, including peers built against older protocol implementations.
    pub fn terminate(&self) {
        if !self.shared.closed.swap(true, atomic::Ordering::AcqRel) {
            self.shared.clear_queues();
        }
        self.shared.connection_waker.wake();
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
        if self.is_closing() {
            return false;
        }
        trace!(frame = %frame, "queuing frame for sending");
        match self.shared.try_queue_frame(frame, None) {
            Ok(()) => true,
            Err((QueueFrameFailure::Closed, _)) => false,
            Err((QueueFrameFailure::Full | QueueFrameFailure::TooLarge, _)) => {
                warn!("failed to queue control frame: connection overloaded");
                self.shared.mark_overloaded();
                false
            }
        }
    }

    /// Queue a liveness control frame ahead of ordinary channel traffic.
    fn send_priority_frame(&self, frame: Frame) -> bool {
        if self.is_closing() {
            return false;
        }
        trace!(frame = %frame, "queuing priority frame for sending");
        match self.shared.try_queue_priority_frame(frame) {
            Ok(()) => true,
            Err((QueueFrameFailure::Closed, _)) => false,
            Err((QueueFrameFailure::Full | QueueFrameFailure::TooLarge, _)) => {
                warn!("failed to queue priority frame: connection overloaded");
                self.shared.mark_overloaded();
                false
            }
        }
    }

    /// Queue a channel data frame, applying backpressure if the outgoing queue is full.
    fn send_data_frame(
        &self,
        frame: Frame,
        waker: &Waker,
    ) -> Result<(), (QueueFrameFailure, Frame)> {
        self.shared.try_queue_frame(frame, Some(waker))
    }

    /// Send a connection command.
    ///
    /// Returns `true` if the command has been successfully queued.
    fn send_cmd(&self, cmd: ConnectionCmd) -> bool {
        if self.is_closing() {
            return false;
        }
        self.shared.queue_cmd(cmd)
    }

    /// Open a new channel over the connection.
    pub async fn open(&mut self, endpoint: &[u8]) -> Result<Channel, OpenError> {
        debug!(
            endpoint = ?std::str::from_utf8(endpoint).ok(),
            "requesting channel open"
        );
        // Channel id will be assigned by the connection when processing the command.
        if self.is_closing() {
            return Err(OpenError::Closed);
        }
        if endpoint.len() > self.shared.limits.max_control_payload_size {
            return Err(OpenError::LimitReached);
        }
        let request = FrameChannelRequest::new(
            ChannelId::NULL,
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
            endpoint,
        );
        let (result_tx, result_rx) = oneshot::channel();
        if !self.send_cmd(ConnectionCmd::OpenChannel { request, result_tx }) {
            return Err(OpenError::Closed);
        }
        let mut cancellation = OpenCancellationGuard {
            shared: self.shared.clone(),
            armed: true,
        };
        let result = match result_rx.await {
            Ok(result) => result,
            Err(_) => Err(OpenError::Closed),
        };
        cancellation.armed = false;
        match &result {
            Ok(_) => debug!("channel opened successfully"),
            Err(e) => debug!(%e, "channel open failed"),
        }
        result
    }
}

/// Wakes the connection task when an outbound-open future is dropped. The connection
/// can then observe the cancelled oneshot sender and release the pending reservation.
struct OpenCancellationGuard {
    shared: Arc<ConnectionShared>,
    armed: bool,
}

impl Drop for OpenCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.connection_waker.wake();
        }
    }
}

/// Error opening a channel.
#[derive(Debug, Clone, Error)]
pub enum OpenError {
    /// The connection has been closed.
    #[error("the connection has been closed")]
    Closed,
    /// A configured connection resource limit has been reached.
    #[error("a connection resource limit has been reached")]
    LimitReached,
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
    /// A valid hello frame has been received.
    hello_received: bool,
    /// Deadline for receiving the peer's initial Hello frame.
    handshake_deadline: Pin<Box<tokio::time::Sleep>>,
    /// Id of the next channel.
    next_channel_id: u64,
    /// Pending requests for opening channels.
    pending_requests: HashMap<ChannelId, oneshot::Sender<Result<Channel, OpenError>>>,
    /// Channels opened over the connection.
    channels: HashMap<ChannelId, ChannelHandle>,
    /// Recently closed channels and their remaining valid receive-credit budget.
    closed_channels: VecDeque<ClosedChannelTombstone>,
    /// Cancelled outbound opens awaiting a late accept or reject.
    cancelled_open_tombstones: VecDeque<ChannelId>,
    /// Interval for pinging the connection.
    ping_interval: tokio::time::Interval,
    /// Last time a ping was sent.
    last_ping: Option<Instant>,
    /// Smoothened estimated round-trip time.
    smoothened_rtt: Option<Duration>,
    /// Indicates whether a pong has been received.
    pong_received: bool,
    /// A liveness Ping is queued, being flushed, or awaiting its Pong.
    ping_outstanding: bool,
    /// The current Ping has been handed to the sink but not yet flushed.
    ping_awaiting_flush: bool,
    /// The transport-write deadline remains active until the Ping is flushed.
    ping_write_deadline_active: bool,
    /// Deadline for handing and flushing the current Ping to the transport.
    ping_write_deadline: Pin<Box<tokio::time::Sleep>>,
    /// The Pong response deadline is active after the Ping has flushed.
    pong_deadline_active: bool,
    /// Deadline for the currently outstanding Ping.
    pong_deadline: Pin<Box<tokio::time::Sleep>>,
    /// Rate counter for peer ping frames.
    peer_ping_rate: RateCounter,
    /// Rate counter for peer channel requests.
    channel_request_rate: RateCounter,
    /// Rate counter for all peer control frames.
    control_frame_rate: RateCounter,
    /// Rate counter for frames received for unknown channels.
    unknown_channel_rate: RateCounter,
    /// Reference to this connection.
    this_ref: ConnectionRef,
}

/// Shared connection state.
#[derive(Debug)]
struct ConnectionShared {
    /// Whether the connection is closing or closed.
    closed: AtomicBool,
    /// Whether an internal queue has exceeded its configured capacity.
    overloaded: AtomicBool,
    /// Waker for the connection event loop.
    connection_waker: AtomicWaker,
    /// Frames and commands waiting to be processed by the connection.
    queues: Mutex<ConnectionQueues>,
    /// Number of incoming channel requests held by callers.
    pending_incoming_requests: atomic::AtomicUsize,
    /// Resource limits for the connection.
    limits: ConnectionLimits,
    /// Smoothened estimated round-trip time.
    smoothened_rtt: RwLock<Option<Duration>>,
    /// Frames sent over the connection.
    frames_sent: AtomicU64,
    /// Frames received over the connection.
    frames_received: AtomicU64,
}

/// Bounded queues shared between a connection and its references.
struct ConnectionQueues {
    /// Frames waiting to be written to the transport.
    frames: VecDeque<Frame>,
    /// Encoded size of all queued frames.
    frame_bytes: usize,
    /// Commands waiting to be handled by the connection.
    commands: VecDeque<ConnectionCmd>,
    /// Channel writers waiting for outgoing frame capacity.
    frame_waiters: Vec<Waker>,
}

impl std::fmt::Debug for ConnectionQueues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionQueues")
            .field("frames", &self.frames.len())
            .field("frame_bytes", &self.frame_bytes)
            .field("commands", &self.commands.len())
            .field("frame_waiters", &self.frame_waiters.len())
            .finish()
    }
}

impl ConnectionQueues {
    /// Create empty queues.
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            frame_bytes: 0,
            commands: VecDeque::new(),
            frame_waiters: Vec::new(),
        }
    }
}

/// Reason why an outgoing frame could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueFrameFailure {
    /// The connection has closed.
    Closed,
    /// The bounded outgoing queue currently has no capacity.
    Full,
    /// The frame exceeds the configured maximum encoded size.
    TooLarge,
}

impl ConnectionShared {
    /// Mark the connection as overloaded and wake its event loop.
    fn mark_overloaded(&self) {
        self.overloaded.store(true, atomic::Ordering::Release);
        self.wake_frame_waiters();
        self.connection_waker.wake();
    }

    /// Queue one outgoing frame if it fits within both queue limits.
    fn try_queue_frame(
        &self,
        frame: Frame,
        waiter: Option<&Waker>,
    ) -> Result<(), (QueueFrameFailure, Frame)> {
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return Err((QueueFrameFailure::Closed, frame));
        }
        let frame_len = frame.as_bytes().len();
        let mut queues = self.queues.lock();
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return Err((QueueFrameFailure::Closed, frame));
        }
        let Some(new_frame_bytes) = queues.frame_bytes.checked_add(frame_len) else {
            return Err((QueueFrameFailure::TooLarge, frame));
        };
        if frame_len > self.limits.max_encoded_frame_size {
            return Err((QueueFrameFailure::TooLarge, frame));
        }
        let (data_frame_limit, data_byte_limit) = if waiter.is_some() {
            (
                self.limits.max_queued_frames - self.limits.reserved_control_queue_frames,
                self.limits.max_queued_frame_bytes - self.limits.reserved_control_queue_bytes,
            )
        } else {
            (
                self.limits.max_queued_frames,
                self.limits.max_queued_frame_bytes,
            )
        };
        if queues.frames.len() >= data_frame_limit || new_frame_bytes > data_byte_limit {
            if let Some(waiter) = waiter
                && !queues
                    .frame_waiters
                    .iter()
                    .any(|queued| queued.will_wake(waiter))
            {
                queues.frame_waiters.push(waiter.clone());
            }
            return Err((QueueFrameFailure::Full, frame));
        }
        queues.frame_bytes = new_frame_bytes;
        queues.frames.push_back(frame);
        drop(queues);
        self.connection_waker.wake();
        Ok(())
    }

    /// Queue a small liveness frame ahead of ordinary traffic while retaining the
    /// same hard frame/byte limits. Data frames cannot consume the reserved capacity.
    fn try_queue_priority_frame(&self, frame: Frame) -> Result<(), (QueueFrameFailure, Frame)> {
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return Err((QueueFrameFailure::Closed, frame));
        }
        let frame_len = frame.as_bytes().len();
        let mut queues = self.queues.lock();
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return Err((QueueFrameFailure::Closed, frame));
        }
        let Some(new_frame_bytes) = queues.frame_bytes.checked_add(frame_len) else {
            return Err((QueueFrameFailure::TooLarge, frame));
        };
        if frame_len > self.limits.max_encoded_frame_size {
            return Err((QueueFrameFailure::TooLarge, frame));
        }
        if queues.frames.len() >= self.limits.max_queued_frames
            || new_frame_bytes > self.limits.max_queued_frame_bytes
        {
            return Err((QueueFrameFailure::Full, frame));
        }
        queues.frame_bytes = new_frame_bytes;
        // The locally queued Hello is a wire-order invariant too. A peer can
        // complete its half of the handshake while our sink is backpressured;
        // never let a priority Ping/Pong overtake that unsent Hello.
        if matches!(queues.frames.front(), Some(Frame::Hello(_))) {
            queues.frames.insert(1, frame);
        } else {
            queues.frames.push_front(frame);
        }
        drop(queues);
        self.connection_waker.wake();
        Ok(())
    }

    /// Pop one outgoing frame and release its queue budget.
    fn pop_frame(&self) -> Option<Frame> {
        let mut queues = self.queues.lock();
        let frame = queues.frames.pop_front()?;
        queues.frame_bytes -= frame.as_bytes().len();
        let waiters = std::mem::take(&mut queues.frame_waiters);
        drop(queues);
        for waiter in waiters {
            waiter.wake();
        }
        Some(frame)
    }

    /// Wake channel writers waiting for outgoing queue capacity.
    fn wake_frame_waiters(&self) {
        let waiters = std::mem::take(&mut self.queues.lock().frame_waiters);
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Queue one connection command if it fits within the command limit.
    fn queue_cmd(&self, cmd: ConnectionCmd) -> bool {
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return false;
        }
        let mut queues = self.queues.lock();
        if self.closed.load(atomic::Ordering::Acquire)
            || self.overloaded.load(atomic::Ordering::Acquire)
        {
            return false;
        }
        if queues.commands.len() >= self.limits.max_queued_commands {
            drop(queues);
            self.mark_overloaded();
            return false;
        }
        queues.commands.push_back(cmd);
        drop(queues);
        self.connection_waker.wake();
        true
    }

    /// Pop one connection command.
    fn pop_cmd(&self) -> Option<ConnectionCmd> {
        self.queues.lock().commands.pop_front()
    }

    /// Drop all queued work.
    fn clear_queues(&self) {
        let mut queues = self.queues.lock();
        let frames = std::mem::take(&mut queues.frames);
        let commands = std::mem::take(&mut queues.commands);
        let waiters = std::mem::take(&mut queues.frame_waiters);
        queues.frame_bytes = 0;
        drop(queues);
        drop(frames);
        drop(commands);
        for waiter in waiters {
            waiter.wake();
        }
    }
}

/// Counter for a fixed one-second rate window.
#[derive(Debug)]
struct RateCounter {
    /// Beginning of the current window.
    started: Instant,
    /// Items observed in the current window.
    count: u32,
}

impl RateCounter {
    /// Create an empty rate counter.
    fn new() -> Self {
        Self {
            started: Instant::now(),
            count: 0,
        }
    }

    /// Record one item, returning whether it remains within the limit.
    fn record(&mut self, limit: u32) -> bool {
        if self.started.elapsed() >= Duration::from_secs(1) {
            self.started = Instant::now();
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count <= limit
    }
}

impl<T> Connection<T> {
    /// Mark the connection and all of its channels as closed.
    fn close_state(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.this_ref
            .shared
            .closed
            .store(true, atomic::Ordering::Release);

        self.this_ref.shared.clear_queues();
        self.pending_requests.clear();

        for (channel, handle) in &self.channels {
            debug!(channel.local_id = channel.0, "closing channel");
            {
                let mut shared = handle.sender_shared.lock();
                shared.closed = true;
                if let Some(waker) = shared.waker.take() {
                    waker.wake();
                }
            }
            {
                let mut shared = handle.receiver_shared.lock();
                shared.closed = true;
                if let Some(waker) = shared.waker.take() {
                    waker.wake();
                }
            }
        }
        self.channels.clear();
        self.closed_channels.clear();
        self.cancelled_open_tombstones.clear();
    }

    /// Release outbound-open reservations whose caller dropped the result future.
    fn cleanup_cancelled_opens(&mut self) -> Result<(), ResourceLimitExceeded> {
        let cancelled = self
            .pending_requests
            .iter()
            .filter_map(|(&channel, result_tx)| result_tx.is_canceled().then_some(channel))
            .collect::<Vec<_>>();
        for channel in cancelled {
            self.pending_requests.remove(&channel);
            if self.cancelled_open_tombstones.len()
                >= self.this_ref.shared.limits.max_closed_channel_tombstones
            {
                return Err(ResourceLimitExceeded(
                    "too many cancelled channel opens are awaiting peer responses",
                ));
            }
            debug!(
                channel.local_id = channel.0,
                "cancelling pending channel open"
            );
            self.cancelled_open_tombstones.push_back(channel);
        }
        Ok(())
    }

    /// Consume a tombstone for a cancelled outbound channel request.
    fn take_cancelled_open(&mut self, local_id: ChannelId) -> bool {
        let Some(index) = self
            .cancelled_open_tombstones
            .iter()
            .position(|channel| *channel == local_id)
        else {
            return false;
        };
        self.cancelled_open_tombstones.remove(index);
        true
    }

    /// Remove channel handles after both local channel halves have been dropped.
    fn cleanup_channels(&mut self) {
        let dropped = self
            .channels
            .iter()
            .filter_map(|(&channel, handle)| {
                let sender_dropped = handle.sender_shared.lock().dropped;
                let receiver = handle.receiver_shared.lock();
                (sender_dropped && receiver.dropped).then_some((
                    channel,
                    receiver.remaining_frame_credit,
                    receiver.remaining_byte_credit,
                ))
            })
            .collect::<Vec<_>>();

        for (channel, remaining_frame_credit, remaining_byte_credit) in dropped {
            debug!(channel.local_id = channel.0, "removing closed channel");
            self.channels.remove(&channel);
            self.closed_channels.push_back(ClosedChannelTombstone {
                local_id: channel,
                remaining_frame_credit,
                remaining_byte_credit,
            });
            while self.closed_channels.len()
                > self.this_ref.shared.limits.max_closed_channel_tombstones
            {
                self.closed_channels.pop_front();
            }
        }
    }

    /// Check whether a channel ID belongs to a recently closed channel.
    fn is_recently_closed_channel(&self, local_id: ChannelId) -> bool {
        self.closed_channels
            .iter()
            .any(|closed| closed.local_id == local_id)
    }

    /// Discard late data for a recently closed channel within its unused receive credits.
    fn discard_late_channel_data(
        &mut self,
        local_id: ChannelId,
        payload_credit: u32,
    ) -> Result<bool, ProtocolViolation> {
        let Some(closed) = self
            .closed_channels
            .iter_mut()
            .find(|closed| closed.local_id == local_id)
        else {
            return Ok(false);
        };
        if closed.remaining_frame_credit == 0 || closed.remaining_byte_credit < payload_credit {
            error!(
                channel.local_id = local_id.0,
                payload_credit,
                remaining_frame_credit = closed.remaining_frame_credit,
                remaining_byte_credit = closed.remaining_byte_credit,
                "protocol violation: late data exceeds closed channel credits"
            );
            return Err(ProtocolViolation(
                "late data exceeds closed channel receive credits",
            ));
        }
        closed.remaining_frame_credit -= 1;
        closed.remaining_byte_credit -= payload_credit;
        trace!(
            channel.local_id = local_id.0,
            payload_credit,
            remaining_frame_credit = closed.remaining_frame_credit,
            remaining_byte_credit = closed.remaining_byte_credit,
            "discarding in-flight data for a recently closed channel"
        );
        Ok(true)
    }
}

impl<T: ConnectionTransport> Connection<T> {
    /// Create a connection from the provided transport.
    pub fn new(transport: T) -> Self {
        Self::with_limits(transport, ConnectionLimits::default())
    }

    /// Create a connection with explicit resource limits.
    ///
    /// # Panics
    ///
    /// Panics when the limits cannot accommodate the legacy initial channel window or
    /// otherwise contain a zero-sized mandatory capacity.
    pub fn with_limits(transport: T, limits: ConnectionLimits) -> Self {
        limits.validate();
        debug!("creating new multiplex connection");
        let shared = Arc::new(ConnectionShared {
            closed: AtomicBool::new(false),
            overloaded: AtomicBool::new(false),
            connection_waker: AtomicWaker::new(),
            queues: Mutex::new(ConnectionQueues::new()),
            pending_incoming_requests: atomic::AtomicUsize::new(0),
            limits,
            smoothened_rtt: RwLock::new(None),
            frames_sent: AtomicU64::new(0),
            frames_received: AtomicU64::new(0),
        });
        let this_ref = ConnectionRef { shared };
        assert!(
            this_ref.send_frame(FrameHello::new(&PROTOCOL_MAGIC, b"").into()),
            "validated connection limits must accommodate the hello frame"
        );
        let mut ping_interval = tokio::time::interval_at(
            tokio::time::Instant::now() + limits.ping_interval,
            limits.ping_interval,
        );
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            transport,
            exhausted: false,
            closed: false,
            hello_received: false,
            handshake_deadline: Box::pin(tokio::time::sleep(limits.handshake_timeout)),
            next_channel_id: 1,
            pending_requests: HashMap::new(),
            channels: HashMap::new(),
            closed_channels: VecDeque::new(),
            cancelled_open_tombstones: VecDeque::new(),
            ping_interval,
            last_ping: None,
            smoothened_rtt: None,
            pong_received: true,
            ping_outstanding: false,
            ping_awaiting_flush: false,
            ping_write_deadline_active: false,
            ping_write_deadline: Box::pin(tokio::time::sleep(limits.ping_write_timeout)),
            pong_deadline_active: false,
            pong_deadline: Box::pin(tokio::time::sleep(limits.pong_timeout)),
            peer_ping_rate: RateCounter::new(),
            channel_request_rate: RateCounter::new(),
            control_frame_rate: RateCounter::new(),
            unknown_channel_rate: RateCounter::new(),
            this_ref,
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
    fn make_channel(
        &mut self,
        local_id: ChannelId,
        remote_id: ChannelId,
        initial_frame_credit: u32,
        initial_byte_credit: u32,
    ) -> Channel {
        assert!(
            self.channels.len() < self.this_ref.shared.limits.max_channels,
            "channel limit must be checked before creating a channel"
        );
        let limits = self.this_ref.shared.limits;
        let (channel, handle) = Channel::new(
            local_id,
            remote_id,
            initial_frame_credit,
            initial_byte_credit,
            limits,
            self.this_ref.clone(),
        );
        self.channels.insert(local_id, handle);
        channel
    }

    /// Number of channel slots currently active or reserved by requests.
    fn occupied_channel_slots(&self) -> usize {
        self.channels.len()
            + self.pending_requests.len()
            + self
                .this_ref
                .shared
                .pending_incoming_requests
                .load(atomic::Ordering::Acquire)
    }

    /// Record a frame for an unknown channel, rejecting a sustained flood.
    fn record_unknown_channel_frame(
        &mut self,
        local_id: ChannelId,
    ) -> Result<(), ProtocolViolation> {
        if !self.unknown_channel_rate.record(
            self.this_ref
                .shared
                .limits
                .max_unknown_channel_frames_per_second,
        ) {
            error!("protocol violation: too many frames for unknown channels");
            return Err(ProtocolViolation("too many frames for unknown channels"));
        }
        if self.unknown_channel_rate.count == 1 {
            warn!(
                channel.local_id = local_id.0,
                "received a frame for an unknown channel"
            );
        } else {
            trace!(
                channel.local_id = local_id.0,
                "received a frame for an unknown channel"
            );
        }
        Ok(())
    }

    /// Handle a connection command.
    fn handle_cmd(&mut self, cmd: ConnectionCmd) {
        match cmd {
            ConnectionCmd::OpenChannel {
                mut request,
                result_tx,
            } => {
                if result_tx.is_canceled() {
                    debug!("discarding a channel open cancelled before dispatch");
                    return;
                }
                let limits = self.this_ref.shared.limits;
                if self.pending_requests.len() >= limits.max_pending_channel_requests
                    || self.occupied_channel_slots() >= limits.max_channels
                {
                    let _ = result_tx.send(Err(OpenError::LimitReached));
                    return;
                }
                let local_id = self.reserve_channel_id();
                debug!(
                    channel.local_id = local_id.0,
                    endpoint = ?std::str::from_utf8(request.endpoint()).ok(),
                    "cmd: opening channel"
                );
                request.set_sender_id(local_id);
                if self.this_ref.send_frame(request.into()) {
                    self.pending_requests.insert(local_id, result_tx);
                } else {
                    let _ = result_tx.send(Err(OpenError::Closed));
                }
            }
            ConnectionCmd::AcceptChannel {
                mut accept,
                initial_frame_credit,
                initial_byte_credit,
                callback,
                pending_slot,
            } => {
                // Convert the pending request into an active channel while the connection
                // exclusively processes commands, so the total slot count cannot race.
                drop(pending_slot);
                let remote_id = accept.receiver_id();
                if self.channels.len() >= self.this_ref.shared.limits.max_channels {
                    warn!(
                        channel.remote_id = remote_id.0,
                        "rejecting channel after reaching the connection channel limit"
                    );
                    self.this_ref.send_frame(
                        FrameChannelReject::new(remote_id, b"connection channel limit reached")
                            .into(),
                    );
                    return;
                }
                let local_id = self.reserve_channel_id();
                debug!(
                    channel.local_id = local_id.0,
                    channel.remote_id = remote_id.0,
                    "cmd: accepting channel"
                );
                accept.set_sender_id(local_id);
                if !self.this_ref.send_frame(accept.into()) {
                    return;
                }
                let channel = self.make_channel(
                    local_id,
                    remote_id,
                    initial_frame_credit,
                    initial_byte_credit,
                );
                callback(channel);
            }
        }
    }

    /// Handle a frame.
    #[tracing::instrument(level = Level::TRACE, skip_all)]
    fn handle_frame(&mut self, frame: Frame) -> Result<Option<ConnectionEvent>, ProtocolViolation> {
        if !self.hello_received && !matches!(&frame, Frame::Hello(_)) {
            error!("protocol violation: hello must be the first peer frame");
            return Err(ProtocolViolation("hello must be the first peer frame"));
        }
        let limits = self.this_ref.shared.limits;
        if matches!(&frame, Frame::ChannelRequest(_))
            && !self
                .channel_request_rate
                .record(limits.max_channel_requests_per_second)
        {
            error!("protocol violation: channel request rate limit exceeded");
            return Err(ProtocolViolation("channel request rate limit exceeded"));
        }
        let control_payload_len = match &frame {
            Frame::Hello(frame) => Some(frame.info().len()),
            Frame::Close(frame) => Some(frame.reason().len()),
            Frame::ChannelRequest(frame) => Some(frame.endpoint().len()),
            Frame::ChannelReject(frame) => Some(frame.reason().len()),
            Frame::ChannelClose(frame) => Some(frame.reason().len()),
            Frame::ChannelClosed(frame) => Some(frame.reason().len()),
            Frame::ChannelAccept(_)
            | Frame::ChannelData(_)
            | Frame::ChannelAdjust(_)
            | Frame::Ping(_)
            | Frame::Pong(_) => None,
        };
        if control_payload_len.is_some_and(|len| len > limits.max_control_payload_size) {
            if let Frame::ChannelRequest(request) = frame {
                warn!(
                    channel.remote_id = request.sender_id().0,
                    "rejecting channel with an oversized endpoint"
                );
                if !self.this_ref.send_frame(
                    FrameChannelReject::new(request.sender_id(), b"channel endpoint too large")
                        .into(),
                ) {
                    return Err(ProtocolViolation("outgoing frame queue limit exceeded"));
                }
                return Ok(None);
            }
            error!("protocol violation: control frame payload is too large");
            return Err(ProtocolViolation("control frame payload is too large"));
        }
        if !matches!(&frame, Frame::ChannelData(_))
            && !self
                .control_frame_rate
                .record(limits.max_control_frames_per_second)
        {
            error!("protocol violation: control frame rate limit exceeded");
            return Err(ProtocolViolation("control frame rate limit exceeded"));
        }
        Ok(match frame {
            Frame::Hello(frame) => {
                if frame.magic() != &PROTOCOL_MAGIC {
                    error!("protocol violation: invalid protocol magic");
                    return Err(ProtocolViolation("invalid protocol magic"));
                }
                if self.hello_received {
                    error!("protocol violation: duplicate hello frame");
                    return Err(ProtocolViolation("duplicate hello frame"));
                }
                self.hello_received = true;
                debug!(info = frame.info(), "connection established");
                Some(ConnectionEvent::Connected)
            }
            Frame::Close(_) => {
                debug!("connection closed");
                self.close_state();
                Some(ConnectionEvent::Closed)
            }
            Frame::ChannelRequest(frame) => {
                debug!(
                    channel.sender_id = frame.sender_id().0,
                    channel.endpoint = frame.endpoint(),
                    "channel requested"
                );
                let pending_incoming = self
                    .this_ref
                    .shared
                    .pending_incoming_requests
                    .load(atomic::Ordering::Acquire);
                if pending_incoming >= limits.max_pending_channel_requests
                    || self.occupied_channel_slots() >= limits.max_channels
                {
                    warn!(
                        channel.remote_id = frame.sender_id().0,
                        "rejecting channel after reaching a connection limit"
                    );
                    if !self.this_ref.send_frame(
                        FrameChannelReject::new(
                            frame.sender_id(),
                            b"connection channel limit reached",
                        )
                        .into(),
                    ) {
                        return Err(ProtocolViolation("outgoing frame queue limit exceeded"));
                    }
                    return Ok(None);
                }
                self.this_ref
                    .shared
                    .pending_incoming_requests
                    .fetch_add(1, atomic::Ordering::AcqRel);
                Some(ConnectionEvent::RequestChannel(ChannelRequest::new(
                    frame,
                    self.make_ref(),
                    true,
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
                let Some(result_tx) = self.pending_requests.remove(&local_id) else {
                    if self.take_cancelled_open(local_id) {
                        debug!(
                            channel.local_id = local_id.0,
                            channel.remote_id = remote_id.0,
                            "closing a channel accepted after its caller cancelled"
                        );
                        if !self.this_ref.send_frame(
                            FrameChannelClose::new(remote_id, b"channel open cancelled").into(),
                        ) || !self.this_ref.send_frame(
                            FrameChannelClosed::new(remote_id, b"channel open cancelled").into(),
                        ) {
                            return Err(ProtocolViolation("outgoing frame queue limit exceeded"));
                        }
                        self.closed_channels.push_back(ClosedChannelTombstone {
                            local_id,
                            remaining_frame_credit: CHANNEL_INITIAL_FRAME_CREDIT,
                            remaining_byte_credit: CHANNEL_INITIAL_BYTE_CREDIT,
                        });
                        while self.closed_channels.len()
                            > self.this_ref.shared.limits.max_closed_channel_tombstones
                        {
                            self.closed_channels.pop_front();
                        }
                        return Ok(None);
                    }
                    error!("protocol violation: channel request not found");
                    return Err(ProtocolViolation("channel request not found"));
                };
                let channel = self.make_channel(
                    local_id,
                    remote_id,
                    frame.frame_credit(),
                    frame.byte_credit(),
                );
                let _ = result_tx.send(Ok(channel));
                None
            }
            Frame::ChannelReject(frame) => {
                let local_id = frame.receiver_id();
                debug!(
                    channel.local_id = local_id.0,
                    reason = frame.reason(),
                    "channel rejected"
                );
                let Some(result_tx) = self.pending_requests.remove(&local_id) else {
                    if self.take_cancelled_open(local_id) {
                        trace!(
                            channel.local_id = local_id.0,
                            "discarding rejection for a cancelled channel open"
                        );
                        return Ok(None);
                    }
                    error!("protocol violation: channel request not found");
                    return Err(ProtocolViolation("channel request not found"));
                };
                let _ = result_tx.send(Err(OpenError::Rejected(Rejection { frame })));
                None
            }
            Frame::ChannelData(frame) => {
                let local_id = frame.receiver_id();
                let payload_len = frame.payload().len();
                let Ok(payload_credit) = u32::try_from(payload_len) else {
                    error!(
                        channel.local_id = local_id.0,
                        payload_len, "protocol violation: channel data frame is too large"
                    );
                    return Err(ProtocolViolation("channel data frame is too large"));
                };
                trace!(channel.local_id = local_id.0, payload_len, "received data");
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.receiver_shared.lock();
                    if shared.remaining_frame_credit == 0 {
                        error!(
                            channel.local_id = local_id.0,
                            "protocol violation: no frame credit remaining"
                        );
                        return Err(ProtocolViolation("no frame credit remaining"));
                    }
                    if shared.remaining_byte_credit < payload_credit {
                        error!(
                            channel.local_id = local_id.0,
                            remaining_byte_credit = shared.remaining_byte_credit,
                            payload_len,
                            "protocol violation: not enough byte credit"
                        );
                        return Err(ProtocolViolation("not enough byte credit"));
                    }
                    shared.remaining_frame_credit -= 1;
                    shared.remaining_byte_credit -= payload_credit;
                    if shared.closed {
                        trace!(
                            channel.local_id = local_id.0,
                            remaining_frame_credit = shared.remaining_frame_credit,
                            remaining_byte_credit = shared.remaining_byte_credit,
                            "discarding in-flight data for closed receiver"
                        );
                        return Ok(None);
                    }
                    trace!(
                        channel.local_id = local_id.0,
                        remaining_frame_credit = shared.remaining_frame_credit,
                        remaining_byte_credit = shared.remaining_byte_credit,
                        buffer_len = shared.buffer.len(),
                        has_waker = shared.waker.is_some(),
                        "buffering data frame"
                    );
                    shared.buffer.push_back(frame);
                    handle
                        .statistics
                        .bytes_received
                        .fetch_add(payload_len as u64, atomic::Ordering::Relaxed);
                    if let Some(waker) = shared.waker.take() {
                        trace!(channel.local_id = local_id.0, "waking receiver");
                        waker.wake();
                    }
                } else {
                    if !self.discard_late_channel_data(local_id, payload_credit)? {
                        self.record_unknown_channel_frame(local_id)?;
                    }
                };
                None
            }
            Frame::ChannelAdjust(frame) => {
                let local_id = frame.receiver_id();
                let add_frame_credit = frame.frame_credit();
                let add_byte_credit = frame.byte_credit();
                if add_frame_credit == 0 && add_byte_credit == 0 {
                    error!("protocol violation: empty channel credit adjustment");
                    return Err(ProtocolViolation("empty channel credit adjustment"));
                }
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.sender_shared.lock();
                    trace!(
                        channel.local_id = local_id.0,
                        add_frame_credit,
                        add_byte_credit,
                        before_frame_credit = shared.remaining_frame_credit,
                        before_byte_credit = shared.remaining_byte_credit,
                        has_waker = shared.waker.is_some(),
                        "adjusting sender credits"
                    );
                    let Some(remaining_frame_credit) =
                        shared.remaining_frame_credit.checked_add(add_frame_credit)
                    else {
                        error!(
                            channel.local_id = local_id.0,
                            "protocol violation: frame credit overflow"
                        );
                        return Err(ProtocolViolation("frame credit overflow"));
                    };
                    let Some(remaining_byte_credit) =
                        shared.remaining_byte_credit.checked_add(add_byte_credit)
                    else {
                        error!(
                            channel.local_id = local_id.0,
                            "protocol violation: byte credit overflow"
                        );
                        return Err(ProtocolViolation("byte credit overflow"));
                    };
                    shared.remaining_frame_credit = remaining_frame_credit;
                    shared.remaining_byte_credit = remaining_byte_credit;
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
                        trace!(
                            channel.local_id = local_id.0,
                            "waking sender after credit adjust"
                        );
                        waker.wake();
                    }
                } else {
                    if !self.is_recently_closed_channel(local_id) {
                        self.record_unknown_channel_frame(local_id)?;
                    }
                }
                None
            }
            Frame::ChannelClose(frame) => {
                let local_id = frame.receiver_id();
                debug!(
                    channel.local_id = local_id.0,
                    reason = ?std::str::from_utf8(frame.reason()).ok(),
                    "channel close received (sender side)"
                );
                if let Some(handle) = self.channels.get_mut(&local_id) {
                    let mut shared = handle.sender_shared.lock();
                    shared.closed = true;
                    if let Some(waker) = shared.waker.take() {
                        waker.wake();
                    }
                } else {
                    if !self.is_recently_closed_channel(local_id) {
                        self.record_unknown_channel_frame(local_id)?;
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
                } else {
                    if !self.is_recently_closed_channel(local_id) {
                        self.record_unknown_channel_frame(local_id)?;
                    }
                }
                None
            }
            Frame::Ping(_) => {
                if !self
                    .peer_ping_rate
                    .record(self.this_ref.shared.limits.max_peer_pings_per_second)
                {
                    error!("protocol violation: ping rate limit exceeded");
                    return Err(ProtocolViolation("ping rate limit exceeded"));
                }
                if !self.this_ref.send_priority_frame(FramePong::new().into()) {
                    return Err(ProtocolViolation("outgoing frame queue limit exceeded"));
                }
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
        if !self.hello_received || self.ping_outstanding {
            return;
        }
        if self.ping_interval.poll_tick(cx).is_ready() {
            self.ping_interval.reset();
            if !self.this_ref.send_priority_frame(FramePing::new().into()) {
                return;
            }
            self.ping_outstanding = true;
            self.ping_write_deadline_active = true;
            self.ping_write_deadline.as_mut().reset(
                tokio::time::Instant::now() + self.this_ref.shared.limits.ping_write_timeout,
            );
            let _ = self.ping_write_deadline.as_mut().poll(cx);
        }
    }

    /// Poll handshake and liveness deadlines, registering the current connection waker.
    fn poll_deadlines(&mut self, cx: &mut task::Context<'_>) -> (bool, bool, bool) {
        let handshake_expired =
            !self.hello_received && self.handshake_deadline.as_mut().poll(cx).is_ready();
        let ping_write_expired = self.ping_write_deadline_active
            && self.ping_write_deadline.as_mut().poll(cx).is_ready();
        let pong_expired = self.pong_deadline_active
            && !self.pong_received
            && self.pong_deadline.as_mut().poll(cx).is_ready();
        (handshake_expired, ping_write_expired, pong_expired)
    }

    /// Handle a pong.
    fn handle_pong(&mut self) -> Result<(), ProtocolViolation> {
        let Some(last_ping) = self.last_ping.take() else {
            return Err(ProtocolViolation("received pong but no ping has been sent"));
        };
        self.pong_received = true;
        self.ping_outstanding = false;
        self.ping_awaiting_flush = false;
        self.ping_write_deadline_active = false;
        self.pong_deadline_active = false;
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
        trace!("poll_event: enter");
        self.this_ref.shared.connection_waker.register(cx.waker());
        if self.closed
            || self.exhausted
            || self.this_ref.shared.closed.load(atomic::Ordering::Acquire)
        {
            return Poll::Ready(Ok(None));
        }
        if self
            .this_ref
            .shared
            .overloaded
            .load(atomic::Ordering::Acquire)
        {
            return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(
                ResourceLimitExceeded("an internal connection queue exceeded its limit"),
            )));
        }
        let (handshake_expired, ping_write_expired, pong_expired) = self.poll_deadlines(cx);
        self.cleanup_channels();
        if let Err(error) = self.cleanup_cancelled_opens() {
            return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(error)));
        }
        self.ping(cx);
        if self
            .this_ref
            .shared
            .overloaded
            .load(atomic::Ordering::Acquire)
        {
            return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(
                ResourceLimitExceeded("the outgoing frame queue exceeded its limit"),
            )));
        }
        // Phase 1: Process incoming commands.
        let max_commands = self.this_ref.shared.limits.max_queued_commands;
        let mut commands_processed = 0;
        while commands_processed < max_commands {
            if let Err(error) = self.cleanup_cancelled_opens() {
                return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(error)));
            }
            let Some(cmd) = self.this_ref.shared.pop_cmd() else {
                break;
            };
            commands_processed += 1;
            self.handle_cmd(cmd);
        }
        if commands_processed == max_commands {
            self.this_ref.shared.connection_waker.wake();
        }
        if self
            .this_ref
            .shared
            .overloaded
            .load(atomic::Ordering::Acquire)
        {
            return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(
                ResourceLimitExceeded("an internal connection queue exceeded its limit"),
            )));
        }
        // Phase 2: Send pending frames over the transport.
        let mut frames_sent_phase2 = 0u32;
        while (frames_sent_phase2 as usize) < self.this_ref.shared.limits.max_queued_frames {
            match self.transport.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => match self.this_ref.shared.pop_frame() {
                    Some(frame) => {
                        let is_liveness_ping = self.ping_outstanding
                            && self.last_ping.is_none()
                            && matches!(&frame, Frame::Ping(_));
                        trace!(frame = %frame, "sending frame over transport");
                        frames_sent_phase2 += 1;
                        self.this_ref
                            .shared
                            .frames_sent
                            .fetch_add(1, atomic::Ordering::Relaxed);
                        if let Err(error) = self.transport.start_send_unpin(frame.into()) {
                            error!(%error, "transport send error");
                            return Poll::Ready(Err(ConnectionError::TransportError(
                                TransportError::SendError(error),
                            )));
                        }
                        if is_liveness_ping {
                            self.last_ping = Some(Instant::now());
                            self.pong_received = false;
                            self.ping_awaiting_flush = true;
                        }
                    }
                    None => {
                        trace!(frames_sent_phase2, "phase 2: no more frames to send");
                        break;
                    }
                },
                Poll::Ready(Err(error)) => {
                    error!(%error, "transport ready error");
                    return Poll::Ready(Err(ConnectionError::TransportError(
                        TransportError::SendError(error),
                    )));
                }
                Poll::Pending => {
                    trace!(
                        frames_sent_phase2,
                        "phase 2: transport not ready, skipping frame_rx poll"
                    );
                    break;
                }
            }
        }
        if (frames_sent_phase2 as usize) == self.this_ref.shared.limits.max_queued_frames {
            self.this_ref.shared.connection_waker.wake();
        }
        // Phase 3: Flush the transport.
        match self.transport.poll_flush_unpin(cx) {
            Poll::Ready(Ok(())) => {
                if self.ping_awaiting_flush {
                    self.ping_awaiting_flush = false;
                    self.ping_write_deadline_active = false;
                    if self.pong_received {
                        self.ping_outstanding = false;
                    } else {
                        self.pong_deadline_active = true;
                        self.pong_deadline.as_mut().reset(
                            tokio::time::Instant::now() + self.this_ref.shared.limits.pong_timeout,
                        );
                        let _ = self.pong_deadline.as_mut().poll(cx);
                    }
                }
            }
            Poll::Ready(Err(error)) => {
                error!(%error, "transport flush error");
                return Poll::Ready(Err(ConnectionError::TransportError(
                    TransportError::SendError(error),
                )));
            }
            Poll::Pending => {}
        }
        // Phase 4: Receive and process incoming frames.
        let mut frames_processed = 0;
        while !self.exhausted
            && frames_processed < self.this_ref.shared.limits.max_frames_processed_per_poll
        {
            match self.transport.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    frames_processed += 1;
                    if frame.len() > self.this_ref.shared.limits.max_encoded_frame_size {
                        error!(
                            frame_len = frame.len(),
                            "connection resource limit exceeded: incoming frame is too large"
                        );
                        return Poll::Ready(Err(ConnectionError::ResourceLimitExceeded(
                            ResourceLimitExceeded("an incoming frame exceeded the size limit"),
                        )));
                    }
                    match Frame::parse(frame) {
                        Ok(frame) => {
                            trace!(frame = %frame, "received frame from transport");
                            self.this_ref
                                .shared
                                .frames_received
                                .fetch_add(1, atomic::Ordering::Relaxed);
                            if handshake_expired
                                && !self.hello_received
                                && !matches!(&frame, Frame::Hello(_))
                            {
                                return Poll::Ready(Err(ConnectionError::DeadlineExceeded(
                                    DeadlineExceeded("peer did not send hello before the deadline"),
                                )));
                            }
                            if pong_expired
                                && !self.pong_received
                                && !matches!(&frame, Frame::Pong(_))
                            {
                                return Poll::Ready(Err(ConnectionError::DeadlineExceeded(
                                    DeadlineExceeded(
                                        "peer did not answer ping before the deadline",
                                    ),
                                )));
                            }
                            if ping_write_expired
                                && self.ping_write_deadline_active
                                && !matches!(&frame, Frame::Pong(_))
                            {
                                return Poll::Ready(Err(ConnectionError::DeadlineExceeded(
                                    DeadlineExceeded(
                                        "transport did not flush ping before the deadline",
                                    ),
                                )));
                            }
                            if let Some(event) = self.handle_frame(frame)? {
                                return Poll::Ready(Ok(Some(event)));
                            }
                        }
                        Err(error) => {
                            error!(%error, "received invalid frame");
                            return Poll::Ready(Err(ConnectionError::ProtocolViolation(
                                ProtocolViolation("invalid frame"),
                            )));
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    error!(%error, "transport recv error");
                    return Poll::Ready(Err(ConnectionError::TransportError(
                        TransportError::RecvError(error),
                    )));
                }
                Poll::Ready(None) => {
                    debug!("transport exhausted (no more frames)");
                    self.exhausted = true;
                    return Poll::Ready(Ok(None));
                }
                Poll::Pending => {
                    break;
                }
            }
        }
        if frames_processed == self.this_ref.shared.limits.max_frames_processed_per_poll {
            self.this_ref.shared.connection_waker.wake();
        }
        if handshake_expired && !self.hello_received {
            return Poll::Ready(Err(ConnectionError::DeadlineExceeded(DeadlineExceeded(
                "peer did not send hello before the deadline",
            ))));
        }
        if ping_write_expired && self.ping_write_deadline_active {
            return Poll::Ready(Err(ConnectionError::DeadlineExceeded(DeadlineExceeded(
                "transport did not flush ping before the deadline",
            ))));
        }
        if pong_expired && !self.pong_received {
            return Poll::Ready(Err(ConnectionError::DeadlineExceeded(DeadlineExceeded(
                "peer did not answer ping before the deadline",
            ))));
        }
        Poll::Pending
    }
}

impl<T: ConnectionTransport> Stream for Connection<T> {
    type Item = Result<ConnectionEvent, ConnectionError<T>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        match self.poll_event(cx) {
            Poll::Ready(Ok(Some(event))) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(Ok(None)) => {
                self.close_state();
                Poll::Ready(None)
            }
            Poll::Ready(Err(error)) => {
                self.close_state();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for Connection<T> {
    fn drop(&mut self) {
        debug!("dropping connection");
        self.close_state();
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
    /// A configured connection resource limit has been exceeded.
    #[error(transparent)]
    ResourceLimitExceeded(#[from] ResourceLimitExceeded),
    /// A connection handshake or liveness deadline was exceeded.
    #[error(transparent)]
    DeadlineExceeded(#[from] DeadlineExceeded),
}

/// Protocol violation.
#[derive(Debug, Error)]
#[error("protocol violation: {0}")]
pub struct ProtocolViolation(&'static str);

/// A configured connection resource limit has been exceeded.
#[derive(Debug, Error)]
#[error("connection resource limit exceeded: {0}")]
pub struct ResourceLimitExceeded(&'static str);

/// A connection handshake or liveness deadline was exceeded.
#[derive(Debug, Error)]
#[error("connection deadline exceeded: {0}")]
pub struct DeadlineExceeded(&'static str);

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
        initial_frame_credit: u32,
        initial_byte_credit: u32,
        callback: Box<dyn Send + FnOnce(Channel)>,
        pending_slot: Option<PendingIncomingSlot>,
    },
}

/// Reservation for one incoming channel request.
#[derive(Debug)]
struct PendingIncomingSlot {
    /// Shared connection state containing the reservation counter.
    shared: Arc<ConnectionShared>,
}

impl Drop for PendingIncomingSlot {
    fn drop(&mut self) {
        let previous = self
            .shared
            .pending_incoming_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
        debug_assert!(previous > 0);
        self.shared.connection_waker.wake();
    }
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
    /// Reservation held until the request is accepted or rejected.
    pending_slot: Option<PendingIncomingSlot>,
}

impl ChannelRequest {
    /// Create a new channel request.
    fn new(
        request: FrameChannelRequest,
        connection: ConnectionRef,
        occupies_pending_slot: bool,
    ) -> Self {
        Self {
            request,
            pending_slot: occupies_pending_slot.then(|| PendingIncomingSlot {
                shared: connection.shared.clone(),
            }),
            connection,
            handled: false,
        }
    }

    /// Mark the request handled.
    fn mark_handled(&mut self) {
        self.handled = true;
    }

    /// Endpoint of the request.
    pub fn endpoint(&self) -> &[u8] {
        self.request.endpoint()
    }

    /// Reject the request.
    fn mut_reject(&mut self, reason: &[u8]) {
        assert!(!self.handled, "request must not be rejected twice");
        self.mark_handled();
        drop(self.pending_slot.take());
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
        self.mark_handled();
        let accept = FrameChannelAccept::new(
            self.request.sender_id(),
            ChannelId::NULL,
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        self.connection.send_cmd(ConnectionCmd::AcceptChannel {
            accept,
            initial_frame_credit: self.request.frame_credit(),
            initial_byte_credit: self.request.byte_credit(),
            callback: Box::new(callback),
            pending_slot: self.pending_slot.take(),
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

/// Channel statistics.
#[derive(Debug, Default)]
pub struct ChannelStatistics {
    /// Number of bytes sent over the channel.
    bytes_sent: AtomicU64,
    /// Number of bytes received over the channel.
    bytes_received: AtomicU64,
}

impl ChannelStatistics {
    /// Estimate the number of bytes sent over the channel.
    pub fn estimate_bytes_sent(&self) -> u64 {
        self.bytes_sent.load(atomic::Ordering::Relaxed)
    }

    /// Estimate the number of bytes received over the channel.
    pub fn estimate_bytes_received(&self) -> u64 {
        self.bytes_received.load(atomic::Ordering::Relaxed)
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
    /// Shared channel statistics.
    statistics: Arc<ChannelStatistics>,
}

/// Bounded state used to distinguish valid late frames from frames for unknown channels.
#[derive(Debug)]
struct ClosedChannelTombstone {
    /// Local ID of the closed channel.
    local_id: ChannelId,
    /// Receive frame credits that may still be in flight.
    remaining_frame_credit: u32,
    /// Receive byte credits that may still be in flight.
    remaining_byte_credit: u32,
}

/// Bi-directional channel.
#[derive(Debug)]
#[pin_project]
pub struct Channel {
    /// Sender.
    #[pin]
    sender: Sender,
    /// Receiver.
    #[pin]
    receiver: Receiver,
    /// Channel statistics.
    statistics: Arc<ChannelStatistics>,
}

impl Channel {
    /// Create a new channel on the given connection with the given ids.
    fn new(
        local_id: ChannelId,
        remote_id: ChannelId,
        initial_frame_credit: u32,
        initial_byte_credit: u32,
        limits: ConnectionLimits,
        connection: ConnectionRef,
    ) -> (Self, ChannelHandle) {
        let statistics = Arc::new(ChannelStatistics::default());
        let channel = Self {
            sender: Sender {
                shared: Arc::new(Mutex::new(SenderShared::new(
                    initial_frame_credit,
                    initial_byte_credit,
                ))),
                remote_id,
                connection: connection.clone(),
                pending: None,
                statistics: statistics.clone(),
                closed_sent: false,
                max_chunk_size: limits.max_channel_payload_size(),
            },
            receiver: Receiver {
                shared: Arc::new(Mutex::new(ReceiverShared::new(
                    limits.max_receive_frame_credit,
                    limits.max_receive_byte_credit,
                ))),
                remote_id,
                local_id,
                connection,
                pending: None,
                offset: 0,
            },
            statistics: statistics.clone(),
        };
        let handle = ChannelHandle {
            local_id,
            remote_id,
            receiver_shared: channel.receiver.shared.clone(),
            sender_shared: channel.sender.shared.clone(),
            statistics,
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
        Self {
            statistics: sender.statistics.clone(),
            sender,
            receiver,
        }
    }

    /// Split the channel into sender and receiver.
    pub fn split(self) -> (Sender, Receiver) {
        (self.sender, self.receiver)
    }

    /// Split the channel into a mutable sender and a mutable receiver.
    pub fn split_mut(&mut self) -> (&mut Sender, &mut Receiver) {
        (&mut self.sender, &mut self.receiver)
    }

    /// Obtain the channel statistics.
    pub fn statistics(&self) -> Arc<ChannelStatistics> {
        self.statistics.clone()
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
    /// Indicates whether the channel is closed for sending.
    closed: bool,
    /// Whether the local sender has been dropped.
    dropped: bool,
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
            closed: false,
            dropped: false,
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
    /// Channel statistics.
    statistics: Arc<ChannelStatistics>,
    /// Whether `ChannelClosed` has already been sent (via explicit close).
    closed_sent: bool,
    /// Maximum payload emitted in one channel data frame.
    max_chunk_size: usize,
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
        if shared.closed || self.connection.is_closing() {
            shared.closed = true;
            trace!(
                channel.remote_id = self.remote_id.0,
                "poll_send_chunk: channel closed"
            );
            return Poll::Ready(Err(ChannelSendError::Closed));
        }
        let Some(chunk) = &self.pending else {
            return Poll::Ready(Ok(()));
        };
        let byte_credit = chunk.frame.payload().len() as u32;
        assert!(shared.remaining_byte_credit >= byte_credit);
        if shared.remaining_frame_credit > 0 {
            let mut frame = self.pending.take().unwrap().frame;
            frame.set_receiver_id(self.remote_id);
            let payload_len = frame.payload().len() as u64;
            match self.connection.send_data_frame(frame.into(), cx.waker()) {
                Ok(()) => {
                    shared.remaining_frame_credit -= 1;
                    shared.remaining_byte_credit -= byte_credit;
                    shared.used_frame_credit += 1;
                    shared.used_byte_credit += byte_credit;
                    trace!(
                        channel.remote_id = self.remote_id.0,
                        payload_len = byte_credit,
                        remaining_frame_credit = shared.remaining_frame_credit,
                        remaining_byte_credit = shared.remaining_byte_credit,
                        "poll_send_chunk: sending chunk"
                    );
                    self.statistics
                        .bytes_sent
                        .fetch_add(payload_len, atomic::Ordering::Relaxed);
                    Poll::Ready(Ok(()))
                }
                Err((QueueFrameFailure::Full, Frame::ChannelData(frame))) => {
                    self.pending = Some(Chunk { frame });
                    trace!(
                        channel.remote_id = self.remote_id.0,
                        "poll_send_chunk: outgoing queue full"
                    );
                    Poll::Pending
                }
                Err((QueueFrameFailure::Full, _)) => unreachable!("queued channel data frame"),
                Err((QueueFrameFailure::TooLarge, _)) => {
                    self.connection.shared.mark_overloaded();
                    shared.closed = true;
                    Poll::Ready(Err(ChannelSendError::ChunkTooLarge))
                }
                Err((QueueFrameFailure::Closed, _)) => {
                    shared.closed = true;
                    Poll::Ready(Err(ChannelSendError::Closed))
                }
            }
        } else {
            trace!(
                channel.remote_id = self.remote_id.0,
                pending_bytes = byte_credit,
                remaining_byte_credit = shared.remaining_byte_credit,
                "poll_send_chunk: no frame credit, pending"
            );
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
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut shared = self.shared.lock();
        if shared.remaining_byte_credit == 0 {
            trace!(
                channel.remote_id = self.remote_id.0,
                remaining_byte_credit = shared.remaining_byte_credit,
                buf_len = buf.len(),
                "poll_write: insufficient byte credit, pending"
            );
            shared.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let chunk_size = (shared.remaining_byte_credit as usize)
            .min(buf.len())
            .min(self.max_chunk_size);
        trace!(
            channel.remote_id = self.remote_id.0,
            buf_len = buf.len(),
            chunk_size,
            remaining_byte_credit = shared.remaining_byte_credit,
            remaining_frame_credit = shared.remaining_frame_credit,
            "poll_write: creating chunk"
        );
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
        if self.closed_sent {
            return Poll::Ready(Ok(()));
        }
        try_ready!(self.as_mut().poll_flush(cx));
        self.closed_sent = true;
        if !self
            .connection
            .send_frame(FrameChannelClosed::new(self.remote_id, b"").into())
        {
            self.shared.lock().closed = true;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection has been closed",
            )));
        }
        self.shared.lock().closed = true;
        Poll::Ready(Ok(()))
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        {
            let mut shared = self.shared.lock();
            shared.closed = true;
            shared.dropped = true;
        }
        if !self.closed_sent && !self.connection.is_closing() {
            debug!(
                channel.remote_id = self.remote_id.0,
                "dropping sender, sending ChannelClosed"
            );
            self.connection
                .send_frame(FrameChannelClosed::new(self.remote_id, b"").into());
        }
        self.connection.shared.connection_waker.wake();
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
    /// Indicates whether the channel is closed for receiving.
    closed: bool,
    /// Whether the local receiver has been dropped.
    dropped: bool,
    /// Optional waker to wake up the receiver when something changed.
    waker: Option<Waker>,
    /// Maximum frame credit.
    max_frame_credit: u32,
    /// Maximum byte credit.
    max_byte_credit: u32,
    /// Configured ceiling for maximum frame credit.
    max_frame_credit_limit: u32,
    /// Configured ceiling for maximum byte credit.
    max_byte_credit_limit: u32,
    /// Remaining frame credit.
    remaining_frame_credit: u32,
    /// Remaining byte credit.
    remaining_byte_credit: u32,
    /// Consumed frame credit not yet returned to the sender.
    pending_frame_credit: u32,
    /// Consumed byte credit not yet returned to the sender.
    pending_byte_credit: u32,
    /// Last time a credit update was sent.
    last_credit_update: Instant,
    /// Estimation of the used bandwidth in bytes per second.
    bandwidth_bytes: Ema,
    /// Estimation of the used bandwidth in frames per second.
    bandwidth_frames: Ema,
}

impl ReceiverShared {
    /// Create a new receiver shared state.
    pub fn new(max_frame_credit_limit: u32, max_byte_credit_limit: u32) -> Self {
        Self {
            buffer: VecDeque::new(),
            closed: false,
            dropped: false,
            waker: None,
            max_frame_credit: CHANNEL_INITIAL_FRAME_CREDIT,
            max_byte_credit: CHANNEL_INITIAL_BYTE_CREDIT,
            max_frame_credit_limit,
            max_byte_credit_limit,
            remaining_frame_credit: CHANNEL_INITIAL_FRAME_CREDIT,
            remaining_byte_credit: CHANNEL_INITIAL_BYTE_CREDIT,
            pending_frame_credit: 0,
            pending_byte_credit: 0,
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
    /// Local id of the channel.
    local_id: ChannelId,
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
        // Important: check the buffer BEFORE checking closed, so that any data
        // received before the close frame is still delivered to the reader.
        if let Some(frame) = shared.buffer.pop_front() {
            let payload_len = u32::try_from(frame.payload().len())
                .expect("accepted channel data fits into a u32");
            shared.pending_frame_credit += 1;
            shared.pending_byte_credit += payload_len;
            trace!(
                channel.local_id = self.local_id.0,
                payload_len,
                remaining_frame_credit = shared.remaining_frame_credit,
                remaining_byte_credit = shared.remaining_byte_credit,
                pending_frame_credit = shared.pending_frame_credit,
                pending_byte_credit = shared.pending_byte_credit,
                max_frame_credit = shared.max_frame_credit,
                max_byte_credit = shared.max_byte_credit,
                buffer_remaining = shared.buffer.len(),
                "poll_next_chunk: consumed frame"
            );
            let frame_credit_low = shared.remaining_frame_credit < shared.max_frame_credit / 2;
            let byte_credit_low = shared.remaining_byte_credit < shared.max_byte_credit / 2;
            let smoothened_rtt = *self.connection.shared.smoothened_rtt.read();
            let old_max_frame_credit = shared.max_frame_credit;
            let old_max_byte_credit = shared.max_byte_credit;
            if frame_credit_low
                && let Some(smoothened_rtt) = smoothened_rtt
                && shared.last_credit_update.elapsed() < 2 * smoothened_rtt
            {
                let old = shared.max_frame_credit;
                shared.max_frame_credit = shared
                    .max_frame_credit
                    .saturating_mul(2)
                    .min(shared.max_frame_credit_limit);
                trace!(
                    channel.local_id = self.local_id.0,
                    old_max = old,
                    new_max = shared.max_frame_credit,
                    "scaling up max frame credit"
                );
            }
            if byte_credit_low
                && let Some(smoothened_rtt) = smoothened_rtt
                && shared.last_credit_update.elapsed() < 2 * smoothened_rtt
            {
                let old = shared.max_byte_credit;
                shared.max_byte_credit = shared
                    .max_byte_credit
                    .saturating_mul(2)
                    .min(shared.max_byte_credit_limit);
                trace!(
                    channel.local_id = self.local_id.0,
                    old_max = old,
                    new_max = shared.max_byte_credit,
                    "scaling up max byte credit"
                );
            }
            let window_grew = shared.max_frame_credit != old_max_frame_credit
                || shared.max_byte_credit != old_max_byte_credit;
            let update_frame_credit =
                frame_credit_low && shared.pending_frame_credit >= shared.max_frame_credit / 2;
            let update_byte_credit =
                byte_credit_low && shared.pending_byte_credit >= shared.max_byte_credit / 2;
            let update_credit = window_grew || update_frame_credit || update_byte_credit;
            if update_credit {
                let consumed_frame_credit = shared.pending_frame_credit;
                let consumed_byte_credit = shared.pending_byte_credit;
                let add_frame_credit =
                    consumed_frame_credit + (shared.max_frame_credit - old_max_frame_credit);
                let add_byte_credit =
                    consumed_byte_credit + (shared.max_byte_credit - old_max_byte_credit);
                trace!(
                    channel.local_id = self.local_id.0,
                    add_frame_credit, add_byte_credit, "sending credit adjustment"
                );
                if self.connection.send_frame(
                    FrameChannelAdjust::new(self.remote_id, add_frame_credit, add_byte_credit)
                        .into(),
                ) {
                    let duration = shared.last_credit_update.elapsed().as_secs_f64();
                    if duration > 0.0 {
                        shared
                            .bandwidth_bytes
                            .update((consumed_byte_credit as f64) / duration);
                        shared
                            .bandwidth_frames
                            .update((consumed_frame_credit as f64) / duration);
                    }
                    shared.last_credit_update = Instant::now();
                    shared.remaining_frame_credit += add_frame_credit;
                    shared.remaining_byte_credit += add_byte_credit;
                    shared.pending_frame_credit = 0;
                    shared.pending_byte_credit = 0;
                    debug_assert!(shared.remaining_frame_credit <= shared.max_frame_credit);
                    debug_assert!(shared.remaining_byte_credit <= shared.max_byte_credit);
                }
            }
            return Poll::Ready(Some(Chunk { frame }));
        }
        if shared.closed {
            trace!(
                channel.local_id = self.local_id.0,
                "poll_next_chunk: channel closed, buffer empty"
            );
            return Poll::Ready(None);
        }
        trace!(
            channel.local_id = self.local_id.0,
            "poll_next_chunk: buffer empty, pending"
        );
        shared.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        {
            let mut shared = self.shared.lock();
            shared.closed = true;
            shared.dropped = true;
            shared.buffer.clear();
            shared.waker = None;
        }
        if self.connection.is_closing() {
            return;
        }
        debug!(
            channel.local_id = self.local_id.0,
            channel.remote_id = self.remote_id.0,
            "dropping receiver, sending ChannelClose"
        );
        self.connection
            .send_frame(FrameChannelClose::new(self.remote_id, b"").into());
        self.connection.shared.connection_waker.wake();
    }
}

impl AsyncRead for Receiver {
    #[tracing::instrument(level = Level::TRACE, skip_all, fields(channel.local_id = self.local_id.0))]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        trace!("poll_read");
        loop {
            if let Some(pending) = &self.pending {
                let bytes = (pending.len() - self.offset).min(buf.len());
                buf[..bytes].copy_from_slice(&pending[self.offset..self.offset + bytes]);
                let pending_len = pending.len();
                self.offset += bytes;
                if self.offset >= pending_len {
                    self.pending = None;
                }
                trace!("returning {bytes} bytes");
                return Poll::Ready(Ok(bytes));
            }
            match self.as_mut().poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    trace!("new chunk available");
                    self.pending = Some(chunk.frame.bytes);
                    self.offset = FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE;
                }
                Poll::Ready(None) => {
                    trace!("channel closed");
                    return Poll::Ready(Ok(0));
                }
                Poll::Pending => {
                    trace!("waiting for new chunk");
                    return Poll::Pending;
                }
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
            match self.as_mut().poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    trace!("new chunk available");
                    self.pending = Some(chunk.frame.bytes);
                    self.offset = FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE;
                }
                Poll::Ready(None) => {
                    trace!("channel closed");
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => {
                    trace!("waiting for new chunk");
                    return Poll::Pending;
                }
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::Sink;
    use tokio::io::AsyncWriteExt as _;

    use super::*;
    use crate::transport::InMemory;

    #[derive(Debug)]
    struct EofTransport;

    impl Stream for EofTransport {
        type Item = Result<EncodedFrame, Infallible>;

        fn poll_next(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    impl Sink<EncodedFrame> for EofTransport {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: EncodedFrame) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct ControlledFlushTransport {
        incoming: VecDeque<EncodedFrame>,
        sent: Arc<Mutex<Vec<EncodedFrame>>>,
        write_ready: Arc<AtomicBool>,
        flush_ready: Arc<AtomicBool>,
    }

    impl Stream for ControlledFlushTransport {
        type Item = Result<EncodedFrame, Infallible>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.incoming
                .pop_front()
                .map_or(Poll::Pending, |frame| Poll::Ready(Some(Ok(frame))))
        }
    }

    impl Sink<EncodedFrame> for ControlledFlushTransport {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.write_ready.load(atomic::Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn start_send(self: Pin<&mut Self>, item: EncodedFrame) -> Result<(), Self::Error> {
            self.sent.lock().push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.flush_ready.load(atomic::Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn in_memory_connection() -> (
        Connection<InMemory<EncodedFrame, EncodedFrame>>,
        InMemory<EncodedFrame, EncodedFrame>,
    ) {
        in_memory_connection_with_limits(ConnectionLimits::default())
    }

    fn in_memory_connection_with_limits(
        limits: ConnectionLimits,
    ) -> (
        Connection<InMemory<EncodedFrame, EncodedFrame>>,
        InMemory<EncodedFrame, EncodedFrame>,
    ) {
        let (mut connection, peer) = raw_in_memory_connection_with_limits(limits);
        connection.hello_received = true;
        (connection, peer)
    }

    fn raw_in_memory_connection_with_limits(
        limits: ConnectionLimits,
    ) -> (
        Connection<InMemory<EncodedFrame, EncodedFrame>>,
        InMemory<EncodedFrame, EncodedFrame>,
    ) {
        let (transport, peer) = InMemory::new_buffered(512);
        (Connection::with_limits(transport, limits), peer)
    }

    async fn receive_peer_frame(peer: &mut InMemory<EncodedFrame, EncodedFrame>) -> Frame {
        let encoded = peer
            .next()
            .await
            .expect("connection dropped its outgoing transport")
            .expect("in-memory transport cannot fail");
        Frame::parse(encoded).expect("connection emitted a malformed frame")
    }

    async fn send_peer_frame(peer: &mut InMemory<EncodedFrame, EncodedFrame>, frame: Frame) {
        peer.send(frame.into())
            .await
            .expect("connection dropped its incoming transport");
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_is_closed_at_the_hello_deadline() {
        let limits = ConnectionLimits {
            handshake_timeout: Duration::from_secs(3),
            ping_interval: Duration::from_secs(60),
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = raw_in_memory_connection_with_limits(limits);
        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());

        tokio::time::advance(limits.handshake_timeout).await;
        let error = next
            .await
            .expect("connection ended without reporting the hello deadline")
            .expect_err("silent peer survived the hello deadline");
        assert!(matches!(error, ConnectionError::DeadlineExceeded(_)));
    }

    #[tokio::test]
    async fn channel_request_before_hello_is_a_protocol_violation() {
        let limits = ConnectionLimits::default();
        let (mut connection, mut peer) = raw_in_memory_connection_with_limits(limits);
        send_peer_frame(
            &mut peer,
            FrameChannelRequest::new(ChannelId(9), 1, 1, b"executor").into(),
        )
        .await;

        let error = connection
            .next()
            .await
            .expect("connection ended without reporting the pre-hello request")
            .expect_err("pre-hello channel request was accepted");
        let ConnectionError::ProtocolViolation(error) = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(error.0, "hello must be the first peer frame");
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_ping_closes_an_established_connection() {
        let limits = ConnectionLimits {
            handshake_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(2),
            pong_timeout: Duration::from_secs(3),
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = raw_in_memory_connection_with_limits(limits);
        send_peer_frame(&mut peer, FrameHello::new(&PROTOCOL_MAGIC, b"peer").into()).await;
        assert!(matches!(
            connection.next().await,
            Some(Ok(ConnectionEvent::Connected))
        ));
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Hello(_)
        ));

        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.ping_interval).await;
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Ping(_)
        ));

        tokio::time::advance(limits.pong_timeout).await;
        let error = next
            .await
            .expect("connection ended without reporting the pong deadline")
            .expect_err("peer survived without answering ping");
        assert!(matches!(error, ConnectionError::DeadlineExceeded(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_hello_and_pong_keep_normal_traffic_working() {
        let limits = ConnectionLimits {
            handshake_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(2),
            pong_timeout: Duration::from_secs(3),
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = raw_in_memory_connection_with_limits(limits);
        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(9)).await;
        send_peer_frame(
            &mut peer,
            FrameHello::new(&PROTOCOL_MAGIC, b"delayed peer").into(),
        )
        .await;
        assert!(matches!(next.await, Some(Ok(ConnectionEvent::Connected))));
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Hello(_)
        ));

        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Ping(_)
        ));
        tokio::time::advance(limits.pong_timeout - Duration::from_millis(1)).await;
        send_peer_frame(&mut peer, FramePong::new().into()).await;
        assert!(futures::poll!(next.as_mut()).is_pending());
        drop(next);

        send_peer_frame(
            &mut peer,
            FrameChannelRequest::new(ChannelId(41), 1, 1, b"executor").into(),
        )
        .await;
        let Some(Ok(ConnectionEvent::RequestChannel(request))) = connection.next().await else {
            panic!("normal traffic did not resume after delayed pong");
        };
        request.reject(b"test complete");
    }

    #[tokio::test(start_paused = true)]
    async fn priority_ping_never_overtakes_a_backpressured_hello() {
        let limits = ConnectionLimits {
            ping_interval: Duration::from_secs(1),
            ping_write_timeout: Duration::from_secs(10),
            ..ConnectionLimits::default()
        };
        let sent = Arc::new(Mutex::new(Vec::new()));
        let write_ready = Arc::new(AtomicBool::new(false));
        let transport = ControlledFlushTransport {
            incoming: VecDeque::from([EncodedFrame::from(Frame::from(FrameHello::new(
                &PROTOCOL_MAGIC,
                b"peer",
            )))]),
            sent: Arc::clone(&sent),
            write_ready: Arc::clone(&write_ready),
            flush_ready: Arc::new(AtomicBool::new(true)),
        };
        let mut connection = Connection::with_limits(transport, limits);
        assert!(matches!(
            connection.next().await,
            Some(Ok(ConnectionEvent::Connected))
        ));

        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.ping_interval).await;
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert!(sent.lock().is_empty());

        write_ready.store(true, atomic::Ordering::Release);
        assert!(futures::poll!(next.as_mut()).is_pending());
        let frames = sent
            .lock()
            .iter()
            .cloned()
            .map(Frame::parse)
            .collect::<Result<Vec<_>, _>>()
            .expect("connection emitted a malformed frame");
        assert!(matches!(frames.first(), Some(Frame::Hello(_))));
        assert!(frames.iter().any(|frame| matches!(frame, Frame::Ping(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn pong_deadline_starts_only_after_ping_flush() {
        let limits = ConnectionLimits {
            ping_interval: Duration::from_secs(1),
            ping_write_timeout: Duration::from_secs(10),
            pong_timeout: Duration::from_secs(2),
            ..ConnectionLimits::default()
        };
        let sent = Arc::new(Mutex::new(Vec::new()));
        let write_ready = Arc::new(AtomicBool::new(true));
        let flush_ready = Arc::new(AtomicBool::new(true));
        let transport = ControlledFlushTransport {
            incoming: VecDeque::from([EncodedFrame::from(Frame::from(FrameHello::new(
                &PROTOCOL_MAGIC,
                b"peer",
            )))]),
            sent: Arc::clone(&sent),
            write_ready,
            flush_ready: Arc::clone(&flush_ready),
        };
        let mut connection = Connection::with_limits(transport, limits);
        assert!(matches!(
            connection.next().await,
            Some(Ok(ConnectionEvent::Connected))
        ));
        flush_ready.store(false, atomic::Ordering::Release);

        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.ping_interval).await;
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert!(
            sent.lock()
                .iter()
                .any(|frame| matches!(Frame::parse(frame.clone()), Ok(Frame::Ping(_))))
        );

        tokio::time::advance(limits.pong_timeout + Duration::from_secs(1)).await;
        assert!(
            futures::poll!(next.as_mut()).is_pending(),
            "Pong timeout ran while the Ping was still unflushed"
        );

        flush_ready.store(true, atomic::Ordering::Release);
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.pong_timeout).await;
        let error = next
            .await
            .expect("connection ended without reporting the Pong deadline")
            .expect_err("unanswered flushed Ping did not expire");
        assert!(matches!(error, ConnectionError::DeadlineExceeded(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn unflushed_ping_hits_distinct_write_deadline() {
        let limits = ConnectionLimits {
            ping_interval: Duration::from_secs(1),
            ping_write_timeout: Duration::from_secs(4),
            pong_timeout: Duration::from_secs(2),
            ..ConnectionLimits::default()
        };
        let flush_ready = Arc::new(AtomicBool::new(true));
        let transport = ControlledFlushTransport {
            incoming: VecDeque::from([EncodedFrame::from(Frame::from(FrameHello::new(
                &PROTOCOL_MAGIC,
                b"peer",
            )))]),
            sent: Arc::new(Mutex::new(Vec::new())),
            write_ready: Arc::new(AtomicBool::new(true)),
            flush_ready: Arc::clone(&flush_ready),
        };
        let mut connection = Connection::with_limits(transport, limits);
        assert!(matches!(
            connection.next().await,
            Some(Ok(ConnectionEvent::Connected))
        ));
        flush_ready.store(false, atomic::Ordering::Release);

        let mut next = Box::pin(connection.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.ping_interval).await;
        assert!(futures::poll!(next.as_mut()).is_pending());
        tokio::time::advance(limits.ping_write_timeout).await;
        let error = next
            .await
            .expect("connection ended without reporting the write deadline")
            .expect_err("unflushed Ping survived the write deadline");
        let ConnectionError::DeadlineExceeded(error) = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(error.0, "transport did not flush ping before the deadline");
    }

    #[tokio::test]
    async fn cancelling_pending_limit_allows_next_open_and_late_reject() {
        let limits = ConnectionLimits {
            max_channels: 4,
            max_pending_channel_requests: 1,
            ping_interval: Duration::from_secs(3600),
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = in_memory_connection_with_limits(limits);
        let mut first_ref = connection.make_ref();
        let mut first_open = Box::pin(first_ref.open(b"first"));
        assert!(futures::poll!(first_open.as_mut()).is_pending());
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Hello(_)
        ));
        let Frame::ChannelRequest(first_request) = receive_peer_frame(&mut peer).await else {
            panic!("first channel request was not sent");
        };
        drop(first_open);

        let mut second_ref = connection.make_ref();
        let mut second_open = Box::pin(second_ref.open(b"second"));
        assert!(futures::poll!(second_open.as_mut()).is_pending());
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        let Frame::ChannelRequest(second_request) = receive_peer_frame(&mut peer).await else {
            panic!("second channel request was not sent after cancellation");
        };

        send_peer_frame(
            &mut peer,
            FrameChannelAccept::new(
                second_request.sender_id(),
                ChannelId(70),
                CHANNEL_INITIAL_FRAME_CREDIT,
                CHANNEL_INITIAL_BYTE_CREDIT,
            )
            .into(),
        )
        .await;
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        let channel = second_open
            .await
            .expect("second open did not complete successfully");

        send_peer_frame(
            &mut peer,
            FrameChannelReject::new(first_request.sender_id(), b"late rejection").into(),
        )
        .await;
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        assert!(!connection.make_ref().is_closing());
        drop(channel);
    }

    #[tokio::test]
    async fn late_accept_for_cancelled_open_is_closed_without_killing_connection() {
        let limits = ConnectionLimits {
            ping_interval: Duration::from_secs(3600),
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = in_memory_connection_with_limits(limits);
        let mut connection_ref = connection.make_ref();
        let mut open = Box::pin(connection_ref.open(b"cancelled"));
        assert!(futures::poll!(open.as_mut()).is_pending());
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Hello(_)
        ));
        let Frame::ChannelRequest(request) = receive_peer_frame(&mut peer).await else {
            panic!("channel request was not sent");
        };
        drop(open);
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }

        let remote_id = ChannelId(88);
        send_peer_frame(
            &mut peer,
            FrameChannelAccept::new(
                request.sender_id(),
                remote_id,
                CHANNEL_INITIAL_FRAME_CREDIT,
                CHANNEL_INITIAL_BYTE_CREDIT,
            )
            .into(),
        )
        .await;
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }

        let first = receive_peer_frame(&mut peer).await;
        let second = receive_peer_frame(&mut peer).await;
        assert!(
            matches!(first, Frame::ChannelClose(ref frame) if frame.receiver_id() == remote_id)
        );
        assert!(
            matches!(second, Frame::ChannelClosed(ref frame) if frame.receiver_id() == remote_id)
        );
        assert!(!connection.make_ref().is_closing());
    }

    #[tokio::test]
    async fn hello_then_channel_open_completes_normally() {
        let limits = ConnectionLimits {
            ping_interval: Duration::from_secs(3600),
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = raw_in_memory_connection_with_limits(limits);

        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        assert!(matches!(
            receive_peer_frame(&mut peer).await,
            Frame::Hello(_)
        ));
        send_peer_frame(&mut peer, FrameHello::new(&PROTOCOL_MAGIC, b"peer").into()).await;
        assert!(matches!(
            connection.next().await,
            Some(Ok(ConnectionEvent::Connected))
        ));

        let mut connection_ref = connection.make_ref();
        let mut open = Box::pin(connection_ref.open(b"executor"));
        assert!(futures::poll!(open.as_mut()).is_pending());
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        let Frame::ChannelRequest(request) = receive_peer_frame(&mut peer).await else {
            panic!("peer did not receive the channel request");
        };
        send_peer_frame(
            &mut peer,
            FrameChannelAccept::new(
                request.sender_id(),
                ChannelId(101),
                CHANNEL_INITIAL_FRAME_CREDIT,
                CHANNEL_INITIAL_BYTE_CREDIT,
            )
            .into(),
        )
        .await;
        {
            let mut event = Box::pin(connection.next());
            assert!(futures::poll!(event.as_mut()).is_pending());
        }
        assert!(open.await.is_ok());
    }

    #[tokio::test]
    async fn transport_eof_terminates_connection_and_pending_open() {
        let mut connection = Connection::new(EofTransport);
        let mut connection_ref = connection.make_ref();
        let (pending_tx, pending_rx) = oneshot::channel();
        connection.pending_requests.insert(ChannelId(1), pending_tx);

        let result = tokio::time::timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("connection did not terminate after transport EOF");

        assert!(result.is_none());
        assert!(pending_rx.await.is_err());
        assert!(connection_ref.is_closing());
        assert!(matches!(
            connection_ref.open(b"endpoint").await,
            Err(OpenError::Closed)
        ));
    }

    #[tokio::test]
    async fn external_termination_wakes_and_closes_the_connection() {
        let (mut connection, _peer) = in_memory_connection();
        let mut connection_ref = connection.make_ref();
        let (pending_tx, pending_rx) = oneshot::channel();
        connection.pending_requests.insert(ChannelId(1), pending_tx);

        connection_ref.terminate();
        connection_ref.terminate();

        let result = tokio::time::timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("terminated connection was not woken");
        assert!(result.is_none());
        assert!(pending_rx.await.is_err());
        assert!(connection_ref.is_closing());
        assert!(matches!(
            connection_ref.open(b"endpoint").await,
            Err(OpenError::Closed)
        ));
    }

    #[tokio::test]
    async fn buffered_frames_consume_the_receive_window() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let _channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );

        for _ in 0..CHANNEL_INITIAL_FRAME_CREDIT {
            let frame = FrameChannelData::new(local_id, b"").into();
            assert!(connection.handle_frame(frame).is_ok());
        }
        let extra_frame = FrameChannelData::new(local_id, b"").into();
        assert!(connection.handle_frame(extra_frame).is_err());
    }

    #[tokio::test]
    async fn buffered_bytes_consume_the_receive_window() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let _channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        let half_window = vec![0; (CHANNEL_INITIAL_BYTE_CREDIT / 2) as usize];

        for _ in 0..2 {
            let frame = FrameChannelData::new(local_id, &half_window).into();
            assert!(connection.handle_frame(frame).is_ok());
        }
        let extra_frame = FrameChannelData::new(local_id, b"x").into();
        assert!(connection.handle_frame(extra_frame).is_err());
    }

    #[tokio::test]
    async fn overflowing_credit_adjust_is_a_protocol_violation() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let _channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );

        let adjust = FrameChannelAdjust::new(local_id, u32::MAX, 0).into();
        assert!(connection.handle_frame(adjust).is_err());
    }

    #[tokio::test]
    async fn channel_accept_uses_peer_advertised_credits() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let (result_tx, result_rx) = oneshot::channel();
        connection.pending_requests.insert(local_id, result_tx);

        let accept = FrameChannelAccept::new(local_id, ChannelId(9), 3, 7).into();
        assert!(connection.handle_frame(accept).is_ok());
        let channel = result_rx
            .await
            .expect("result sender dropped")
            .expect("channel rejected");
        let shared = channel.sender.shared.lock();
        assert_eq!(shared.remaining_frame_credit, 3);
        assert_eq!(shared.remaining_byte_credit, 7);
    }

    #[tokio::test]
    async fn channel_request_uses_peer_advertised_credits() {
        let (mut connection, _peer) = in_memory_connection();
        let request = ChannelRequest::new(
            FrameChannelRequest::new(ChannelId(42), 5, 11, b"endpoint"),
            connection.make_ref(),
            false,
        );
        let (channel_tx, channel_rx) = oneshot::channel();
        request.accept(move |channel| {
            let _ = channel_tx.send(channel);
        });

        let cmd = connection
            .this_ref
            .shared
            .pop_cmd()
            .expect("accept command was not queued");
        connection.handle_cmd(cmd);
        let channel = channel_rx.await.expect("accept callback was not called");
        let shared = channel.sender.shared.lock();
        assert_eq!(shared.remaining_frame_credit, 5);
        assert_eq!(shared.remaining_byte_credit, 11);
    }

    #[tokio::test]
    async fn dropped_channels_are_removed_from_the_connection() {
        let (mut connection, _peer) = in_memory_connection();
        let channel = connection.make_channel(
            ChannelId(1),
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        drop(channel);

        connection.cleanup_channels();
        assert!(connection.channels.is_empty());
        assert_eq!(connection.closed_channels.len(), 1);
    }

    #[tokio::test]
    async fn closed_receiver_still_enforces_receive_credits() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        let (sender, receiver) = channel.split();
        drop(receiver);

        for _ in 0..CHANNEL_INITIAL_FRAME_CREDIT {
            assert!(
                connection
                    .handle_frame(FrameChannelData::new(local_id, b"").into())
                    .is_ok()
            );
        }
        assert!(
            connection
                .handle_frame(FrameChannelData::new(local_id, b"").into())
                .is_err()
        );
        drop(sender);
    }

    #[tokio::test]
    async fn valid_late_data_for_a_cleaned_up_channel_is_not_unknown() {
        let (mut connection, _peer) = in_memory_connection();
        let local_id = ChannelId(1);
        let channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        {
            let handle = connection.channels.get(&local_id).expect("channel missing");
            let mut receiver = handle.receiver_shared.lock();
            receiver.max_frame_credit = 256;
            receiver.max_byte_credit = 256;
            receiver.remaining_frame_credit = 256;
            receiver.remaining_byte_credit = 256;
        }
        drop(channel);
        connection.cleanup_channels();

        for _ in 0..256 {
            assert!(
                connection
                    .handle_frame(FrameChannelData::new(local_id, b"x").into())
                    .is_ok()
            );
        }
        assert_eq!(connection.unknown_channel_rate.count, 0);
        assert!(
            connection
                .handle_frame(FrameChannelData::new(local_id, b"x").into())
                .is_err()
        );
    }

    #[tokio::test]
    async fn sender_fails_after_connection_is_dropped() {
        let (mut connection, _peer) = in_memory_connection();
        let mut channel = connection.make_channel(
            ChannelId(1),
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        drop(connection);

        let error = channel
            .write_all(b"lost data")
            .await
            .expect_err("write unexpectedly succeeded");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn sender_makes_progress_with_a_small_advertised_window() {
        let (mut connection, _peer) = in_memory_connection();
        let mut channel = connection.make_channel(ChannelId(1), ChannelId(2), 1, 1);

        channel.write_all(b"x").await.expect("write failed");
        channel.flush().await.expect("flush failed");

        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        let data = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("data frame was not queued");
        assert!(matches!(data, Frame::ChannelData(_)));
    }

    #[tokio::test]
    async fn sender_shutdown_is_idempotent_and_prevents_more_writes() {
        let (mut connection, _peer) = in_memory_connection();
        let mut channel = connection.make_channel(
            ChannelId(1),
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );

        channel.shutdown().await.expect("first shutdown failed");
        channel.shutdown().await.expect("second shutdown failed");
        let error = channel
            .write_all(b"after shutdown")
            .await
            .expect_err("write after shutdown unexpectedly succeeded");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn outgoing_queue_limit_closes_the_connection() {
        let limits = ConnectionLimits {
            max_queued_frames: 1,
            reserved_control_queue_frames: 0,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let connection_ref = connection.make_ref();

        assert!(!connection_ref.send_frame(FramePing::new().into()));
        assert!(connection_ref.is_closing());
        let error = connection
            .next()
            .await
            .expect("connection ended without reporting queue overload")
            .expect_err("queue overload was not reported");
        assert!(matches!(error, ConnectionError::ResourceLimitExceeded(_)));
    }

    #[tokio::test]
    async fn full_outgoing_queue_applies_backpressure_to_channel_data() {
        let limits = ConnectionLimits {
            max_queued_frames: 1,
            reserved_control_queue_frames: 0,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let connection_ref = connection.make_ref();
        let mut channel = connection.make_channel(ChannelId(1), ChannelId(2), 2, 2);
        channel.write_all(b"x").await.expect("write failed");

        let mut flush = Box::pin(channel.flush());
        assert!(futures::poll!(flush.as_mut()).is_pending());
        assert!(!connection_ref.is_closing());

        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        assert!(futures::poll!(flush.as_mut()).is_ready());
        assert!(!connection_ref.is_closing());
        assert!(matches!(
            connection.this_ref.shared.pop_frame(),
            Some(Frame::ChannelData(_))
        ));
    }

    #[tokio::test]
    async fn outgoing_queue_reserves_capacity_for_control_frames() {
        let limits = ConnectionLimits {
            max_queued_frames: 2,
            reserved_control_queue_frames: 1,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let connection_ref = connection.make_ref();
        let mut channel = connection.make_channel(ChannelId(1), ChannelId(2), 2, 2);
        channel.write_all(b"x").await.expect("write failed");

        let mut flush = Box::pin(channel.flush());
        assert!(futures::poll!(flush.as_mut()).is_pending());
        assert!(connection_ref.send_frame(FramePing::new().into()));
        assert!(!connection_ref.is_closing());

        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        assert!(futures::poll!(flush.as_mut()).is_pending());
        assert!(matches!(
            connection.this_ref.shared.pop_frame(),
            Some(Frame::Ping(_))
        ));
        assert!(futures::poll!(flush.as_mut()).is_ready());
    }

    #[tokio::test]
    async fn command_queue_limit_closes_the_connection() {
        let limits = ConnectionLimits {
            max_queued_commands: 1,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let connection_ref = connection.make_ref();
        let (first_result_tx, _first_result_rx) = oneshot::channel();
        let (second_result_tx, _second_result_rx) = oneshot::channel();

        assert!(connection_ref.send_cmd(ConnectionCmd::OpenChannel {
            request: FrameChannelRequest::new(ChannelId::NULL, 1, 1, b"first"),
            result_tx: first_result_tx,
        }));
        assert!(!connection_ref.send_cmd(ConnectionCmd::OpenChannel {
            request: FrameChannelRequest::new(ChannelId::NULL, 1, 1, b"second"),
            result_tx: second_result_tx,
        }));
        let error = connection
            .next()
            .await
            .expect("connection ended without reporting queue overload")
            .expect_err("queue overload was not reported");
        assert!(matches!(error, ConnectionError::ResourceLimitExceeded(_)));
    }

    #[tokio::test]
    async fn excess_pending_channel_requests_are_rejected() {
        let limits = ConnectionLimits {
            max_channels: 2,
            max_pending_channel_requests: 1,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);

        let first = FrameChannelRequest::new(ChannelId(10), 1, 1, b"first").into();
        let event = connection
            .handle_frame(first)
            .expect("first request failed")
            .expect("first request was unexpectedly rejected");
        let ConnectionEvent::RequestChannel(first_request) = event else {
            panic!("expected a channel request");
        };

        let second = FrameChannelRequest::new(ChannelId(11), 1, 1, b"second").into();
        assert!(
            connection
                .handle_frame(second)
                .expect("second request caused a protocol error")
                .is_none()
        );
        assert_eq!(
            connection
                .this_ref
                .shared
                .pending_incoming_requests
                .load(atomic::Ordering::Acquire),
            1
        );

        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        let reject = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("excess request was not rejected");
        let Frame::ChannelReject(reject) = reject else {
            panic!("expected a channel rejection");
        };
        assert_eq!(reject.receiver_id(), ChannelId(11));

        drop(first_request);
        assert_eq!(
            connection
                .this_ref
                .shared
                .pending_incoming_requests
                .load(atomic::Ordering::Acquire),
            0
        );
    }

    #[tokio::test]
    async fn channel_requests_are_rejected_at_the_active_channel_limit() {
        let limits = ConnectionLimits {
            max_channels: 1,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let _channel = connection.make_channel(
            ChannelId(1),
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );

        let request = FrameChannelRequest::new(ChannelId(10), 1, 1, b"excess").into();
        assert!(
            connection
                .handle_frame(request)
                .expect("excess request caused a protocol error")
                .is_none()
        );
        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        assert!(matches!(
            connection.this_ref.shared.pop_frame(),
            Some(Frame::ChannelReject(_))
        ));
    }

    #[tokio::test]
    async fn peer_ping_rate_is_limited() {
        let limits = ConnectionLimits {
            max_peer_pings_per_second: 2,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);

        assert!(connection.handle_frame(FramePing::new().into()).is_ok());
        assert!(connection.handle_frame(FramePing::new().into()).is_ok());
        let error = connection
            .handle_frame(FramePing::new().into())
            .expect_err("excess ping was accepted");
        assert_eq!(error.0, "ping rate limit exceeded");
    }

    #[tokio::test]
    async fn oversized_channel_endpoint_is_rejected() {
        let limits = ConnectionLimits {
            max_control_payload_size: 4,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let request = FrameChannelRequest::new(ChannelId(10), 1, 1, b"12345").into();

        assert!(
            connection
                .handle_frame(request)
                .expect("oversized endpoint caused a protocol error")
                .is_none()
        );
        let _hello = connection
            .this_ref
            .shared
            .pop_frame()
            .expect("hello frame was not queued");
        assert!(matches!(
            connection.this_ref.shared.pop_frame(),
            Some(Frame::ChannelReject(_))
        ));
    }

    #[tokio::test]
    async fn oversized_incoming_frame_is_rejected_before_parsing() {
        let limits = ConnectionLimits {
            max_encoded_frame_size: FrameChannelData::<Vec<u8>>::MIN_FRAME_SIZE
                + CHANNEL_INITIAL_BYTE_CREDIT as usize,
            ..ConnectionLimits::default()
        };
        let (mut connection, mut peer) = in_memory_connection_with_limits(limits);
        peer.send(Bytes::from(vec![0; limits.max_encoded_frame_size + 1]))
            .await
            .expect("failed to send oversized frame");

        let error = connection
            .next()
            .await
            .expect("connection ended without reporting oversized frame")
            .expect_err("oversized frame was accepted");
        assert!(matches!(error, ConnectionError::ResourceLimitExceeded(_)));
    }

    #[tokio::test]
    async fn adaptive_receive_window_stops_at_configured_limit() {
        let limits = ConnectionLimits {
            max_receive_frame_credit: CHANNEL_INITIAL_FRAME_CREDIT,
            max_receive_byte_credit: 2 * CHANNEL_INITIAL_BYTE_CREDIT,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let local_id = ChannelId(1);
        let mut channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        *connection.this_ref.shared.smoothened_rtt.write() = Some(Duration::from_secs(1));

        let first_payload = vec![0; CHANNEL_INITIAL_BYTE_CREDIT as usize];
        connection
            .handle_frame(FrameChannelData::new(local_id, &first_payload).into())
            .expect("first data frame was rejected");
        channel
            .receiver
            .next()
            .await
            .expect("first chunk was not delivered");
        assert_eq!(
            channel.receiver.shared.lock().max_byte_credit,
            2 * CHANNEL_INITIAL_BYTE_CREDIT
        );

        let second_payload = vec![0; (2 * CHANNEL_INITIAL_BYTE_CREDIT) as usize];
        connection
            .handle_frame(FrameChannelData::new(local_id, &second_payload).into())
            .expect("second data frame was rejected");
        channel
            .receiver
            .next()
            .await
            .expect("second chunk was not delivered");
        assert_eq!(
            channel.receiver.shared.lock().max_byte_credit,
            2 * CHANNEL_INITIAL_BYTE_CREDIT
        );
    }

    #[tokio::test]
    async fn adaptive_receive_window_growth_saturates() {
        let limits = ConnectionLimits {
            max_receive_frame_credit: u32::MAX,
            max_receive_byte_credit: u32::MAX,
            ..ConnectionLimits::default()
        };
        let (mut connection, _peer) = in_memory_connection_with_limits(limits);
        let local_id = ChannelId(1);
        let mut channel = connection.make_channel(
            local_id,
            ChannelId(2),
            CHANNEL_INITIAL_FRAME_CREDIT,
            CHANNEL_INITIAL_BYTE_CREDIT,
        );
        *connection.this_ref.shared.smoothened_rtt.write() = Some(Duration::from_secs(1));
        {
            let mut receiver = channel.receiver.shared.lock();
            receiver.max_frame_credit = u32::MAX / 2 + 1;
            receiver.max_byte_credit = u32::MAX / 2 + 1;
            receiver.remaining_frame_credit = 0;
            receiver.remaining_byte_credit = 0;
            receiver
                .buffer
                .push_back(FrameChannelData::new(local_id, b""));
        }

        channel
            .receiver
            .next()
            .await
            .expect("chunk was not delivered");
        let receiver = channel.receiver.shared.lock();
        assert_eq!(receiver.max_frame_credit, u32::MAX);
        assert_eq!(receiver.max_byte_credit, u32::MAX);
    }
}
