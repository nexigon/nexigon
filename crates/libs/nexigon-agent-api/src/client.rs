//! Client for the agent local API Unix socket.
//!
//! Lets in-host code execute hub actions through the agent's existing
//! connection instead of opening a fresh hub link of its own.

use std::io;
use std::path::Path;

use nexigon_api::Action;
use nexigon_api::types::errors::ActionError;
use nexigon_client::Execute;
use nexigon_rpc::ExecuteError;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;
use tokio::net::UnixStream;

use crate::MAGIC;
use crate::MAX_HANDSHAKE_LEN;
use crate::VERSION;
use crate::types::handshake::ClientHello;
use crate::types::handshake::ServerError;
use crate::types::handshake::ServerHello;

/// Connect to the agent's local API and request the `executor` endpoint.
///
/// Performs the handshake defined by [`nexigon_agent_api`] and returns a
/// [`LocalExecutor`] that speaks `nexigon-rpc` over the resulting stream.
pub async fn connect_local_executor(
    socket_path: &Path,
) -> Result<LocalExecutor, LocalConnectError> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let hello = ClientHello {
        version: VERSION,
        endpoint: "executor".to_owned(),
        options: None,
    };
    write_hello(&mut stream, &hello).await?;
    match read_hello(&mut stream).await? {
        ServerHello::Ok(_) => {
            let (rx, tx) = tokio::io::split(stream);
            Ok(LocalExecutor { rx, tx })
        }
        ServerHello::Error(error) => Err(LocalConnectError::Rejected(error)),
    }
}

/// Executor that runs hub actions over the agent local API.
pub struct LocalExecutor {
    rx: ReadHalf<UnixStream>,
    tx: WriteHalf<UnixStream>,
}

impl LocalExecutor {
    /// Execute an [`Action`] and return its result.
    ///
    /// The outer `Result` reports framing-level failures (transport,
    /// malformed responses); the inner `Result` reports the hub-side
    /// [`ActionError`].
    pub async fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        nexigon_rpc::execute(&action, &mut self.rx, &mut self.tx).await
    }
}

impl Execute for LocalExecutor {
    async fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        LocalExecutor::execute(self, action).await
    }
}

/// Error establishing a [`LocalExecutor`] connection.
#[derive(Debug, Error)]
pub enum LocalConnectError {
    /// I/O error on the underlying Unix socket.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Server returned a non-matching magic prefix.
    #[error("magic mismatch: expected {expected:?}, got {actual:?}")]
    MagicMismatch {
        /// Magic the agent should have sent.
        expected: [u8; 4],
        /// Magic actually received.
        actual: [u8; 4],
    },
    /// Server replied with a payload exceeding [`MAX_HANDSHAKE_LEN`].
    #[error("server hello exceeds maximum size: {len} > {max}", max = MAX_HANDSHAKE_LEN)]
    HelloTooLarge {
        /// Length the server announced.
        len: u32,
    },
    /// Server hello could not be parsed as JSON.
    #[error("malformed server hello: {0}")]
    MalformedHello(#[source] serde_json::Error),
    /// Agent rejected the handshake (returned a [`ServerHello::Error`]).
    #[error("agent rejected handshake: {} ({:?})", _0.message, _0.code)]
    Rejected(ServerError),
}

async fn write_hello(stream: &mut UnixStream, hello: &ClientHello) -> io::Result<()> {
    let body = serde_json::to_vec(hello).expect("ClientHello serialization is infallible");
    stream.write_all(&MAGIC).await?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_hello(stream: &mut UnixStream) -> Result<ServerHello, LocalConnectError> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await?;
    if magic != MAGIC {
        return Err(LocalConnectError::MagicMismatch {
            expected: MAGIC,
            actual: magic,
        });
    }
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_HANDSHAKE_LEN {
        return Err(LocalConnectError::HelloTooLarge { len });
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(LocalConnectError::MalformedHello)
}
