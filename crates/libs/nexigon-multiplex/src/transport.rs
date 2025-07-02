//! Abstractions enabling different transport layers.
//!
//! Both receiving and sending may result in an error. The connection is terminated when
//! an error occurs on the transport layer.

use std::error::Error;
use std::pin::Pin;
use std::task;
use std::task::Poll;

use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStream;
use futures::channel::mpsc;
use thiserror::Error;

/// Never type.
pub type Never = std::convert::Infallible;

/// Abstraction for bidirectional message-based transport layers.
pub trait Transport<In, Out>
where
    Self: TryStream<Ok = In, Error = Self::RecvError>
        + Stream<Item = Result<In, <Self as TryStream>::Error>>
        + Sink<Out, Error = Self::SendError>,
{
    /// Error receiving a message.
    type RecvError: 'static + Send + Sync + Error;
    /// Error sending a message.
    type SendError: 'static + Send + Sync + Error;
}

impl<T, In, Out> Transport<In, Out> for T
where
    T: TryStream<Ok = In> + Stream<Item = Result<In, <Self as TryStream>::Error>> + Sink<Out>,
    <T as TryStream>::Error: 'static + Send + Sync + Error,
    <T as Sink<Out>>::Error: 'static + Send + Sync + Error,
{
    type RecvError = <T as TryStream>::Error;
    type SendError = <T as Sink<Out>>::Error;
}

/// Error receiving or sending a message.
#[derive(Debug, Error)]
pub enum TransportError<RecvError, SendError> {
    /// Error receiving a message.
    #[error("recv error: {0}")]
    RecvError(RecvError),
    /// Error sending a message.
    #[error("send error: {0}")]
    SendError(SendError),
}

/// In-memory transport layer using channels.
#[derive(Debug)]
pub struct InMemory<In, Out> {
    /// Channel for sending messages.
    sender: mpsc::Sender<Out>,
    /// Channel for receiving messages.
    receiver: mpsc::Receiver<In>,
}

impl<In, Out> InMemory<In, Out> {
    /// Create an in-memory transport layer.
    pub fn new() -> (Self, InMemory<Out, In>) {
        Self::new_buffered(0)
    }

    /// Create an in-memory transport layer with additional buffer space.
    pub fn new_buffered(buffer: usize) -> (Self, InMemory<Out, In>) {
        let (x_sender, x_receiver) = mpsc::channel(buffer);
        let (y_sender, y_receiver) = mpsc::channel(buffer);
        (
            InMemory {
                sender: x_sender,
                receiver: y_receiver,
            },
            InMemory {
                sender: y_sender,
                receiver: x_receiver,
            },
        )
    }
}

// It suffices to implement `Stream` here to implement `Transport` as `TryStream` has a
// blanked implementation for any stream yielding `Result`.
impl<In, Out> Stream for InMemory<In, Out> {
    type Item = Result<In, Never>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_next_unpin(cx).map(|item| item.map(Ok))
    }
}

/// Error sending a message over an in-memory transport layer.
///
/// This error is generated iff the receiver has been dropped.
#[derive(Debug, Clone, Error)]
#[error("unable to send message, no receiver")]
pub struct InMemorySendError(());

/// Convert the error returned by the channel to [`InMemorySendError`].
fn convert_poll_send_error<T>(
    polled: Poll<Result<T, mpsc::SendError>>,
) -> Poll<Result<T, InMemorySendError>> {
    polled.map(|ready| {
        ready.map_err(|error| {
            // Another error should never happen with the stream interface.
            debug_assert!(error.is_disconnected());
            InMemorySendError(())
        })
    })
}

impl<In, Out> Sink<Out> for InMemory<In, Out> {
    type Error = InMemorySendError;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        convert_poll_send_error(self.sender.poll_ready_unpin(cx))
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: Out) -> Result<(), Self::Error> {
        self.sender
            .start_send_unpin(item)
            .map_err(|_| InMemorySendError(()))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        convert_poll_send_error(self.sender.poll_flush_unpin(cx))
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        convert_poll_send_error(self.sender.poll_close_unpin(cx))
    }
}

// Compilation should fail when `InMemory` does not implement `Transport`.
static_assertions::assert_impl_all!(InMemory<Vec<u8>, Vec<u8>>: Transport<Vec<u8>, Vec<u8>>);
