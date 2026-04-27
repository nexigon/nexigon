//! Nexigon Agent library.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use nexigon_client::ClientIdentity;
use nexigon_client::ClientToken;
use nexigon_client::WebsocketConnection;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_ids::ids::DeviceId;
use tokio::sync::oneshot;
use tracing::info;

pub use nexigon_client::install_crypto_provider;

pub mod config;
pub mod handlers;
pub mod system_info;
#[cfg(target_os = "linux")]
pub mod terminal;

mod run;

pub use run::run;
pub use run::run_with_connection;

use crate::config::Config;

/// Load and parse the agent configuration from the given path.
///
/// Returns the parsed config and the canonicalised parent directory of the
/// config file, which is used as the base for resolving relative paths
/// (certificates, fingerprint script, command directory).
pub async fn load_config(config_path: &Path) -> anyhow::Result<(Arc<Config>, PathBuf)> {
    let config_path = config_path
        .canonicalize()
        .context("cannot canonicalize config path")?;
    let Some(config_dir) = config_path.parent() else {
        bail!("config path has no parent");
    };
    let config = toml::from_str::<Config>(
        &tokio::fs::read_to_string(&config_path)
            .await
            .context("cannot read config")?,
    )
    .context("cannot parse config")?;
    Ok((Arc::new(config), config_dir.to_path_buf()))
}

/// Establish a hub connection using the agent's configuration.
///
/// Generates a self-signed client certificate if one does not yet exist at
/// the configured path. `register_connection` controls whether the hub
/// should treat this as a long-running registered device (true for the
/// `run` command, false for one-shot CLI invocations).
pub async fn connect(
    config: &Config,
    config_dir: &Path,
    register_connection: bool,
) -> anyhow::Result<WebsocketConnection> {
    let cert_path = config_dir.join(
        config
            .ssl_cert
            .as_deref()
            .unwrap_or(Path::new("/etc/nexigon/agent/ssl/cert.pem")),
    );
    let key_path = config_dir.join(
        config
            .ssl_key
            .as_deref()
            .unwrap_or(Path::new("/etc/nexigon/agent/ssl/key.pem")),
    );
    if !cert_path.exists() {
        if key_path.exists() {
            bail!("found SSL key but certificate is missing");
        }
        info!(?cert_path, "generating SSL certificate and key");
        let (certificate, key) = nexigon_cert::generate_self_signed_certificate();
        if let Some(parent) = cert_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        if let Some(parent) = key_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&cert_path, certificate.to_pem()).await?;
        tokio::fs::write(&key_path, key).await?;
    }
    let fingerprint_data =
        tokio::process::Command::new(config_dir.join(&config.fingerprint_script))
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .output()
            .await
            .context("running fingerprint script")?
            .stdout;
    let fingerprint = DeviceFingerprint::from_data(&fingerprint_data);
    let cert = tokio::fs::read_to_string(&cert_path)
        .await
        .context("cannot read certificate")?;
    let key = tokio::fs::read_to_string(&key_path)
        .await
        .context("cannot read private key")?;
    let identity = ClientIdentity::from_pem(&cert, &key).context("cannot parse identity")?;
    let connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().context("cannot parse hub URL")?,
        ClientToken::DeploymentToken(config.token.clone()),
    )
    .with_identity(Some(identity))
    .with_device_fingerprint(Some(fingerprint))
    .with_register_connection(register_connection)
    .dangerous_with_disable_tls(config.dangerous_disable_tls.unwrap_or(false))
    .connect()
    .await
    .context("cannot connect to Nexigon Hub")?;
    Ok(connection)
}

/// Handle for an agent running in the background.
///
/// Returned by [`spawn`]. The agent runs until either [`AgentHandle::stop`]
/// is called or the connection to the hub is lost.
pub struct AgentHandle {
    /// Resolves to the device id once the agent has registered with the hub.
    pub ready: oneshot::Receiver<DeviceId>,
    /// Join handle for the background task running the agent.
    pub join: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// Sender that, when fired or dropped, signals the agent to shut down.
    pub shutdown: oneshot::Sender<()>,
}

impl AgentHandle {
    /// Signal the agent to shut down and wait for it to finish.
    pub async fn stop(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        self.join.await.context("agent task panicked")?
    }
}

/// Spawn an agent on the current Tokio runtime.
///
/// The returned [`AgentHandle`] exposes the device id (once the agent has
/// registered) and a shutdown channel. The caller must have already
/// installed a Rustls crypto provider via [`install_crypto_provider`].
pub fn spawn(config_path: PathBuf) -> AgentHandle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = oneshot::channel::<DeviceId>();
    let join = tokio::spawn(async move {
        let (config, config_dir) = load_config(&config_path).await?;
        let connection = connect(&config, &config_dir, true).await?;
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        run_with_connection(config, &config_dir, connection, shutdown, Some(ready_tx)).await
    });
    AgentHandle {
        ready: ready_rx,
        join,
        shutdown: shutdown_tx,
    }
}
