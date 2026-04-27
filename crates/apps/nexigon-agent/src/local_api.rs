//! Unix-socket-based local API for in-host clients.
//!
//! Listens on a Unix socket and accepts handshakes defined by the
//! [`nexigon_agent_api`] crate. The only endpoint supported in this
//! revision is `"executor"`, which is bridged byte-for-byte to a hub-side
//! executor channel — local clients can speak `nexigon-rpc` directly to the
//! hub over the agent's existing connection.

#![cfg(unix)]

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Context;
use nexigon_agent_api::MAGIC;
use nexigon_agent_api::MAX_HANDSHAKE_LEN;
use nexigon_agent_api::VERSION;
use nexigon_agent_api::types::handshake::ClientHello;
use nexigon_agent_api::types::handshake::ServerError;
use nexigon_agent_api::types::handshake::ServerErrorCode;
use nexigon_agent_api::types::handshake::ServerHello;
use nexigon_agent_api::types::handshake::ServerOk;
use nexigon_multiplex::ConnectionRef;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::LocalApiConfig;

/// Default path of the local API Unix socket.
pub const DEFAULT_SOCKET_PATH: &str = "/run/nexigon/agent/control/socket.sock";

/// File mode applied to the socket after binding.
const SOCKET_MODE: u32 = 0o660;

/// Serve the agent local API until `shutdown` resolves.
///
/// Binds the Unix socket described by `config`, accepts connections, and
/// dispatches each one to the endpoint requested in its `ClientHello`.
/// The socket is removed when this function returns.
///
/// `hub_ref` is cloned per accepted connection to open hub-side channels.
/// A failure on a single client is logged and does not affect the listener.
pub async fn serve(
    config: &LocalApiConfig,
    hub_ref: ConnectionRef,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let socket_path = config
        .socket_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating local API socket dir {parent:?}"))?;
    }
    if socket_path.exists() {
        match UnixStream::connect(&socket_path).await {
            Ok(_) => {
                anyhow::bail!(
                    "local API socket {socket_path:?} is already in use; refusing to start"
                );
            }
            Err(_) => {
                tokio::fs::remove_file(&socket_path)
                    .await
                    .with_context(|| format!("removing stale socket {socket_path:?}"))?;
            }
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding local API socket {socket_path:?}"))?;
    let mut perms = tokio::fs::metadata(&socket_path).await?.permissions();
    perms.set_mode(SOCKET_MODE);
    tokio::fs::set_permissions(&socket_path, perms).await?;
    info!(path = %socket_path.display(), "agent local API listening");
    let _guard = SocketGuard {
        path: socket_path.clone(),
    };

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                debug!("local API shutdown signaled");
                return Ok(());
            }
            res = listener.accept() => {
                let (stream, _) = res.context("accepting local API connection")?;
                let hub_ref = hub_ref.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, hub_ref).await {
                        warn!("local API client error: {e:#}");
                    }
                });
            }
        }
    }
}

async fn handle_client(mut stream: UnixStream, mut hub_ref: ConnectionRef) -> anyhow::Result<()> {
    let mut magic = [0u8; 4];
    stream
        .read_exact(&mut magic)
        .await
        .context("reading magic")?;
    anyhow::ensure!(
        magic == MAGIC,
        "magic mismatch: expected {MAGIC:?}, got {magic:?}",
    );
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("reading hello length")?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(
        len <= MAX_HANDSHAKE_LEN,
        "client hello too large: {len} > {MAX_HANDSHAKE_LEN}",
    );
    let mut body = vec![0u8; len as usize];
    stream
        .read_exact(&mut body)
        .await
        .context("reading hello body")?;
    let hello: ClientHello = match serde_json::from_slice(&body) {
        Ok(hello) => hello,
        Err(e) => {
            return reject(
                &mut stream,
                ServerErrorCode::InvalidRequest,
                format!("malformed client hello: {e}"),
            )
            .await;
        }
    };
    if hello.version != VERSION {
        return reject(
            &mut stream,
            ServerErrorCode::UnsupportedVersion,
            format!(
                "agent speaks version {VERSION}, client requested {}",
                hello.version,
            ),
        )
        .await;
    }
    match hello.endpoint.as_str() {
        "executor" => {
            send_hello(&mut stream, ServerHello::Ok(ServerOk { version: VERSION })).await?;
            let mut hub_channel = hub_ref
                .open(b"executor")
                .await
                .context("opening hub executor channel")?;
            tokio::io::copy_bidirectional(&mut stream, &mut hub_channel)
                .await
                .context("splicing executor channel to hub")?;
            Ok(())
        }
        other => {
            reject(
                &mut stream,
                ServerErrorCode::UnknownEndpoint,
                format!("unknown endpoint {other:?}"),
            )
            .await
        }
    }
}

async fn reject(
    stream: &mut UnixStream,
    code: ServerErrorCode,
    message: String,
) -> anyhow::Result<()> {
    send_hello(stream, ServerHello::Error(ServerError { code, message })).await
}

async fn send_hello(stream: &mut UnixStream, hello: ServerHello) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&hello).expect("ServerHello serialization is infallible");
    stream.write_all(&MAGIC).await?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), "failed to remove socket: {e}");
        }
    }
}
