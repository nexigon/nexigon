//! Simple RPC protocol for executing actions over arbitrary transports.

use bytes::BufMut;
use bytes::BytesMut;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tracing::Level;
use tracing::debug;
use tracing::trace;

use nexigon_api::Action;
use nexigon_api::types::errors::ActionError;
use nexigon_api::types::errors::ActionResult;

/// Maximum action name size (255 bytes).
const MAX_ACTION_NAME_SIZE: u16 = 255;

/// Maximum action input size (8 MiB).
const MAX_INPUT_SIZE: u32 = 8 * 1024 * 1024;

/// Maximum action output size (8 MiB).
const MAX_OUTPUT_SIZE: u32 = 8 * 1024 * 1024;

/// Execute an action over the given transport.
#[tracing::instrument(level = Level::DEBUG, skip_all, fields(action.name = A::NAME))]
pub async fn execute<A: Action, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    action: &A,
    mut rx: R,
    mut tx: W,
) -> Result<Result<A::Output, ActionError>, ExecuteError> {
    debug!("executing action");
    trace!(?action);
    let mut buffer = BytesMut::new();
    buffer.put_u16(A::NAME.len() as u16);
    buffer.put_slice(A::NAME.as_bytes());
    let input = serde_json::to_vec(&action).unwrap();
    buffer.put_u32(input.len() as u32);
    buffer.put_slice(&input);
    let (_, output) = tokio::try_join!(
        async {
            tx.write_all(&buffer).await.map_err(ExecuteError::Read)?;
            tx.flush().await.map_err(ExecuteError::Read)?;
            trace!("done sending action");
            Ok(())
        },
        async {
            let mut output_size = [0u8; 4];
            rx.read_exact(&mut output_size)
                .await
                .map_err(ExecuteError::Read)?;
            let output_size = u32::from_be_bytes(output_size);
            trace!(output_size);
            if output_size > MAX_OUTPUT_SIZE {
                return Err(ExecuteError::OutputTooLarge(output_size));
            }
            let mut output = vec![0u8; output_size as usize];
            rx.read_exact(&mut output)
                .await
                .map_err(ExecuteError::Read)?;
            trace!("done receiving output");
            Ok(output)
        }
    )?;
    serde_json::from_slice::<ActionResult<A::Output>>(&output)
        .map_err(ExecuteError::MalformedOutput)
        .map(Into::into)
}

/// Error executing an action over a transport.
#[derive(Debug, Error)]
pub enum ExecuteError {
    /// Error reading from the transport.
    #[error("error reading from transport")]
    Read(#[source] std::io::Error),
    /// Error writing to the transport.
    #[error("error writing to transport")]
    Write(#[source] std::io::Error),
    /// Malformed output.
    #[error("malformed output")]
    MalformedOutput(#[source] serde_json::Error),
    /// Output exceeds maximum size.
    #[error("output exceeds maximum size ({} > {})", .0, MAX_OUTPUT_SIZE)]
    OutputTooLarge(u32),
}

/// Read an action from the given transport.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn read_action<R: AsyncRead + Unpin>(mut rx: R) -> Result<SerializedAction, ReadError> {
    debug!("receiving action");
    let mut name_size = [0u8; 2];
    rx.read_exact(&mut name_size)
        .await
        .map_err(ReadError::Read)?;
    let name_size = u16::from_be_bytes(name_size);
    trace!(name_size);
    if name_size > MAX_ACTION_NAME_SIZE {
        return Err(ReadError::ActionNameTooLarge(name_size));
    }
    let mut name = vec![0u8; name_size as usize];
    rx.read_exact(&mut name).await.map_err(ReadError::Read)?;
    let name = String::from_utf8(name).map_err(ReadError::InvalidActionName)?;
    trace!(name);
    let mut input_size = [0u8; 4];
    rx.read_exact(&mut input_size)
        .await
        .map_err(ReadError::Read)?;
    let input_size = u32::from_be_bytes(input_size);
    if input_size > MAX_INPUT_SIZE {
        return Err(ReadError::ActionInputTooLarge(input_size));
    }
    let mut input = vec![0u8; input_size as usize];
    rx.read_exact(&mut input).await.map_err(ReadError::Read)?;
    debug!(action_name = name, "action has been received");
    Ok(SerializedAction { name, input })
}

/// Serialized action.
#[derive(Debug)]
pub struct SerializedAction {
    /// Action name.
    pub name: String,
    /// Action input.
    pub input: Vec<u8>,
}

/// Error reading an action.
#[derive(Debug, Error)]
pub enum ReadError {
    /// Error reading from the transport.
    #[error("error reading from transport")]
    Read(#[source] std::io::Error),
    /// Action name is not valid UTF-8.
    #[error("action name is not valid UTF-8")]
    InvalidActionName(#[source] std::string::FromUtf8Error),
    /// Action name exceeds maximum size.
    #[error("action name exceeds maximum size ({} > {})", .0, MAX_ACTION_NAME_SIZE)]
    ActionNameTooLarge(u16),
    /// Action input exceeds maximum size.
    #[error("action input exceeds maximum size ({} > {})", .0, MAX_INPUT_SIZE)]
    ActionInputTooLarge(u32),
}

/// Write action result to the given transport.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn write_action_result<T: Serialize, W: AsyncWrite + Unpin>(
    result: ActionResult<T>,
    mut tx: W,
) -> Result<(), WriteError> {
    let result = serde_json::to_vec(&result).map_err(WriteError::Serialization)?;
    debug!("sending action result");
    tx.write_all(&(result.len() as u32).to_be_bytes())
        .await
        .map_err(WriteError::Write)?;
    tx.write_all(&result).await.map_err(WriteError::Write)?;
    tx.flush().await.map_err(WriteError::Write)?;
    debug!("done sending action result");
    Ok(())
}

/// Error writing an action result.
#[derive(Debug, Error)]
pub enum WriteError {
    /// Error writing to the transport.
    #[error("error writing to transport")]
    Write(#[source] std::io::Error),
    /// Error serializing the action result.
    #[error("error serializing action result")]
    Serialization(#[source] serde_json::Error),
}
