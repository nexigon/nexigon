//! Websocket-based transport for [`nexigon_multiplex`].

use std::pin::Pin;
use std::task;
use std::task::Poll;

use bytes::Bytes;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::ready;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Websocket transport for [`nexigon_multiplex`].
#[derive(Debug)]
pub struct WebSocketTransport<S> {
    /// Underlying websocket.
    socket: WebSocketStream<S>,
}

impl<S> WebSocketTransport<S> {
    /// Create a new [`WebSocketTransport`].
    pub fn new(socket: WebSocketStream<S>) -> Self {
        Self { socket }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream for WebSocketTransport<S> {
    type Item = Result<Bytes, tokio_tungstenite::tungstenite::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.socket.poll_next_unpin(cx)) {
                Some(Ok(message)) => {
                    match message {
                        Message::Text(_) => { /* ignore */ }
                        Message::Binary(frame) => {
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Message::Ping(_) => { /* ignore */ }
                        Message::Pong(_) => { /* ignore */ }
                        Message::Close(_) => { /* ignore */ }
                        Message::Frame(_) => { /* ignore */ }
                    }
                }
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                None => return std::task::Poll::Ready(None),
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
