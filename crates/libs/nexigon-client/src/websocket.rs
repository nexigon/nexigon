//! Websocket-based transport for [`nexigon_multiplex`].

use std::pin::Pin;
use std::task;
use std::task::Poll;

use bytes::Bytes;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Maximum number of control messages consumed in one stream poll.
const MAX_CONTROL_MESSAGES_PER_POLL: usize = 32;

/// Websocket receive error.
#[derive(Debug, Error)]
pub enum WebSocketReceiveError {
    /// Error from the Websocket implementation.
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    /// Text is not part of the multiplex transport contract.
    #[error("received a text WebSocket message on the binary multiplex transport")]
    UnexpectedText,
    /// Raw frames are an internal tungstenite detail and must never reach the transport.
    #[error("received an unexpected raw WebSocket frame on the multiplex transport")]
    UnexpectedRawFrame,
}

/// Websocket transport for [`nexigon_multiplex`].
#[derive(Debug)]
pub struct WebSocketTransport<S> {
    /// Underlying websocket.
    socket: WebSocketStream<S>,
    /// A control response is queued and must be flushed before reading more input.
    flush_control_response: bool,
    /// The peer initiated a clean close handshake.
    peer_closed: bool,
    /// The receive side has terminated or returned a fatal contract error.
    receive_terminated: bool,
}

impl<S> WebSocketTransport<S> {
    /// Create a new [`WebSocketTransport`].
    pub fn new(socket: WebSocketStream<S>) -> Self {
        Self {
            socket,
            flush_control_response: false,
            peer_closed: false,
            receive_terminated: false,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream for WebSocketTransport<S> {
    type Item = Result<Bytes, WebSocketReceiveError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        if self.receive_terminated {
            return Poll::Ready(None);
        }

        let mut control_messages = 0;
        loop {
            if self.flush_control_response {
                match self.socket.poll_flush_unpin(cx) {
                    Poll::Ready(Ok(())) => self.flush_control_response = false,
                    Poll::Ready(Err(error)) => {
                        self.receive_terminated = true;
                        return Poll::Ready(Some(Err(error.into())));
                    }
                    Poll::Pending => return Poll::Pending,
                }

                if self.peer_closed {
                    self.receive_terminated = true;
                    return Poll::Ready(None);
                }
            }

            if control_messages == MAX_CONTROL_MESSAGES_PER_POLL {
                // The underlying stream can have arbitrarily many control messages buffered.
                // Reschedule instead of monopolizing the executor while looking for data.
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            match self.socket.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(message))) => {
                    match message {
                        Message::Text(_) => {
                            self.receive_terminated = true;
                            return Poll::Ready(Some(Err(WebSocketReceiveError::UnexpectedText)));
                        }
                        Message::Binary(frame) => {
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Message::Ping(_) => {
                            // Tungstenite queued the matching pong while reading the ping. Flush
                            // it before another ping can replace that automatic response.
                            self.flush_control_response = true;
                            control_messages += 1;
                        }
                        Message::Pong(_) => {
                            control_messages += 1;
                        }
                        Message::Close(_) => {
                            // Tungstenite queued the matching close response. End the byte stream
                            // only after the response has been flushed.
                            self.peer_closed = true;
                            self.flush_control_response = true;
                            control_messages += 1;
                        }
                        Message::Frame(_) => {
                            self.receive_terminated = true;
                            return Poll::Ready(Some(Err(
                                WebSocketReceiveError::UnexpectedRawFrame,
                            )));
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    self.receive_terminated = true;
                    return Poll::Ready(Some(Err(error.into())));
                }
                Poll::Ready(None) => {
                    self.receive_terminated = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Sink<Bytes> for WebSocketTransport<S> {
    type Error = tokio_tungstenite::tungstenite::Error;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.socket.poll_ready_unpin(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.socket.start_send_unpin(Message::Binary(item))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.socket.poll_flush_unpin(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.socket.poll_close_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;

    use bytes::Bytes;
    use futures::SinkExt;
    use futures::Stream;
    use futures::StreamExt;
    use futures::task::ArcWake;
    use futures::task::waker_ref;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    use super::MAX_CONTROL_MESSAGES_PER_POLL;
    use super::WebSocketReceiveError;
    use super::WebSocketTransport;

    async fn websocket_pair() -> (
        WebSocketTransport<DuplexStream>,
        WebSocketStream<DuplexStream>,
    ) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client, server) = tokio::join!(
            WebSocketStream::from_raw_socket(client_io, Role::Client, None),
            WebSocketStream::from_raw_socket(server_io, Role::Server, None),
        );
        (WebSocketTransport::new(client), server)
    }

    #[tokio::test]
    async fn binary_messages_are_transport_data() {
        let (mut transport, mut peer) = websocket_pair().await;
        let data = Bytes::from_static(b"multiplex frame");
        peer.send(Message::Binary(data.clone())).await.unwrap();

        assert_eq!(transport.next().await.unwrap().unwrap(), data);
    }

    #[tokio::test]
    async fn text_messages_are_fatal_contract_errors() {
        let (mut transport, mut peer) = websocket_pair().await;
        peer.send(Message::Text("not multiplex data".into()))
            .await
            .unwrap();

        assert!(matches!(
            transport.next().await,
            Some(Err(WebSocketReceiveError::UnexpectedText))
        ));
        assert!(transport.next().await.is_none());
    }

    #[tokio::test]
    async fn peer_close_is_acknowledged_and_ends_the_transport() {
        let (mut transport, mut peer) = websocket_pair().await;
        peer.send(Message::Close(None)).await.unwrap();

        assert!(transport.next().await.is_none());
        assert!(matches!(peer.next().await, Some(Ok(Message::Close(None)))));
        assert!(transport.next().await.is_none());
    }

    #[tokio::test]
    async fn ping_is_answered_before_following_binary_data() {
        let (mut transport, mut peer) = websocket_pair().await;
        let ping_data = Bytes::from_static(b"health");
        let binary_data = Bytes::from_static(b"data");
        peer.feed(Message::Ping(ping_data.clone())).await.unwrap();
        peer.feed(Message::Binary(binary_data.clone()))
            .await
            .unwrap();
        peer.flush().await.unwrap();

        assert_eq!(transport.next().await.unwrap().unwrap(), binary_data);
        assert_eq!(
            peer.next().await.unwrap().unwrap(),
            Message::Pong(ping_data)
        );
    }

    #[tokio::test]
    async fn pong_and_repeated_control_messages_do_not_hide_binary_data() {
        let (mut transport, mut peer) = websocket_pair().await;
        for _ in 0..(MAX_CONTROL_MESSAGES_PER_POLL * 3) {
            peer.feed(Message::Pong(Bytes::new())).await.unwrap();
        }
        let binary_data = Bytes::from_static(b"after controls");
        peer.feed(Message::Binary(binary_data.clone()))
            .await
            .unwrap();
        peer.flush().await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), transport.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(received, binary_data);
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl ArcWake for WakeCounter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn control_message_batch_yields_to_the_executor() {
        let (mut transport, mut peer) = websocket_pair().await;
        for _ in 0..MAX_CONTROL_MESSAGES_PER_POLL {
            peer.feed(Message::Pong(Bytes::new())).await.unwrap();
        }
        let binary_data = Bytes::from_static(b"next poll");
        peer.feed(Message::Binary(binary_data.clone()))
            .await
            .unwrap();
        peer.flush().await.unwrap();

        let wake_counter = Arc::new(WakeCounter::default());
        let waker = waker_ref(&wake_counter);
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut transport).poll_next(&mut context),
            Poll::Pending
        ));
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            Pin::new(&mut transport).poll_next(&mut context),
            Poll::Ready(Some(Ok(data))) if data == binary_data
        ));
    }
}
