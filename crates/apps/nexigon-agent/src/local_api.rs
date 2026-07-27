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
use nexigon_agent_api::DEFAULT_SOCKET_PATH;
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
use tokio::task::JoinSet;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::LocalApiConfig;

/// File mode applied to the socket after binding.
const SOCKET_MODE: u32 = 0o660;
const MAX_CONCURRENT_LOCAL_API_CLIENTS: usize = 32;

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
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                debug!("local API shutdown signaled");
                break;
            }
            result = clients.join_next(), if !clients.is_empty() => {
                match result {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => warn!(?error, "local API client failed"),
                    Some(Err(error)) => warn!(?error, "local API client task panicked"),
                    None => {}
                }
            }
            res = listener.accept() => {
                let (stream, _) = res.context("accepting local API connection")?;
                if clients.len() >= MAX_CONCURRENT_LOCAL_API_CLIENTS {
                    warn!("local API connection limit reached; rejecting client");
                    drop(stream);
                    continue;
                }
                let hub_ref = hub_ref.clone();
                clients.spawn(handle_client(stream, hub_ref));
            }
        }
    }

    clients.abort_all();
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            warn!(?error, "local API client task failed during shutdown");
        }
    }
    Ok(())
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
            let mut hub_channel = match hub_ref.open(b"executor").await {
                Ok(channel) => channel,
                Err(error) => {
                    warn!(?error, "hub executor channel is unavailable");
                    return reject(
                        &mut stream,
                        ServerErrorCode::Internal,
                        "hub executor is unavailable".to_owned(),
                    )
                    .await;
                }
            };
            send_hello(&mut stream, ServerHello::Ok(ServerOk { version: VERSION })).await?;
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
    let body = serde_json::to_vec(&hello).context("serializing server hello")?;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use futures::StreamExt;
    use nexigon_multiplex::Connection;
    use nexigon_multiplex::ConnectionEvent;
    use nexigon_multiplex::transport::InMemory;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn shutdown_removes_listener_and_awaits_idle_clients() {
        let root = tempdir().unwrap();
        let socket_path = root.path().join("agent.sock");
        let config = LocalApiConfig::new().with_socket_path(Some(socket_path.clone()));
        let (_peer_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(8);
        let agent_connection = Connection::new(agent_transport);
        let hub_ref = agent_connection.make_ref();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            serve(&config, hub_ref, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });
        let mut idle_client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("cannot connect to local API: {error}"),
                }
            }
        })
        .await
        .expect("local API listener did not start");

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("local API did not stop")
            .expect("local API task panicked")
            .expect("local API failed");
        assert!(!socket_path.exists(), "local API socket was left behind");
        let mut byte = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(1), idle_client.read(&mut byte))
            .await
            .expect("idle client was not closed");
        assert!(
            matches!(closed, Ok(0))
                || matches!(closed, Err(ref error) if error.kind() == std::io::ErrorKind::ConnectionReset),
            "idle client remained usable: {closed:?}"
        );
    }

    #[tokio::test]
    async fn upstream_rejection_is_reported_before_local_success() {
        let (hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(8);
        let mut hub_connection = Connection::new(hub_transport);
        let mut agent_connection = Connection::new(agent_transport);
        let hub_ref = agent_connection.make_ref();
        let hub_driver = tokio::spawn(async move {
            while let Some(event) = hub_connection.next().await {
                match event {
                    Ok(ConnectionEvent::Connected) => {}
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        request.reject(b"executor unavailable");
                    }
                    Ok(ConnectionEvent::Closed) | Err(_) => break,
                }
            }
        });
        let agent_driver = tokio::spawn(async move {
            while let Some(event) = agent_connection.next().await {
                if !matches!(event, Ok(ConnectionEvent::Connected)) {
                    break;
                }
            }
        });

        let (mut client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_client(server, hub_ref));
        let hello = ClientHello::new(VERSION, "executor".to_owned());
        let body = serde_json::to_vec(&hello).unwrap();
        client.write_all(&MAGIC).await.unwrap();
        client
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(&body).await.unwrap();
        client.flush().await.unwrap();

        let mut magic = [0u8; 4];
        client.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic, MAGIC);
        let len = client.read_u32().await.unwrap() as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).await.unwrap();
        let response: ServerHello = serde_json::from_slice(&body).unwrap();
        let ServerHello::Error(error) = response else {
            panic!("local API acknowledged an unavailable upstream executor");
        };
        assert!(matches!(error.code, ServerErrorCode::Internal));
        assert_eq!(error.message, "hub executor is unavailable");

        handler
            .await
            .expect("local API handler panicked")
            .expect("local API handler failed");
        hub_driver.abort();
        agent_driver.abort();
        let _ = hub_driver.await;
        let _ = agent_driver.await;
    }
}
