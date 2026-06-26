//! Nexigon Agent library.

use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use jiff::Timestamp;
use nexigon_client::ClientIdentity;
use nexigon_client::ClientToken;
use nexigon_client::WebsocketConnection;
use nexigon_ids::ids::DeploymentToken;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_ids::ids::DeviceId;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tracing::info;

pub use nexigon_client::install_crypto_provider;

pub mod config;
pub mod handlers;
#[cfg(unix)]
pub mod local_api;
pub mod provisioning;
pub mod system_info;
#[cfg(target_os = "linux")]
pub mod terminal;

mod run;

pub use run::run;
pub use run::run_with_connection;

use crate::config::Config;

/// Default directory for persistent agent data.
pub const DEFAULT_DATA_PATH: &str = "/var/lib/nexigon/agent";

const CREDENTIALS_FILE_NAME: &str = "credentials.json";

/// Credentials persisted after successful pairing-key provisioning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCredentials {
    /// Hub URL selected during pairing.
    pub hub_url: String,
    /// Device-bound deployment token issued by the hub.
    pub deployment_token: DeploymentToken,
    /// Timestamp at which the credentials were written.
    pub paired_at: Timestamp,
}

impl AgentCredentials {
    fn resolved(&self) -> ResolvedCredentials {
        ResolvedCredentials {
            hub_url: self.hub_url.clone(),
            token: self.deployment_token.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedCredentials {
    hub_url: String,
    token: DeploymentToken,
}

#[derive(Debug)]
pub(crate) struct DeviceIdentity {
    client_identity: ClientIdentity,
    pub(crate) fingerprint: DeviceFingerprint,
}

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

/// Return the configured persistent data directory.
pub fn data_path(config: &Config, config_dir: &Path) -> PathBuf {
    config_dir.join(
        config
            .data_path
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_DATA_PATH)),
    )
}

/// Return the provisioned-credentials path.
pub fn credentials_path(config: &Config, config_dir: &Path) -> PathBuf {
    data_path(config, config_dir).join(CREDENTIALS_FILE_NAME)
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
    let Some(credentials) = resolve_credentials(config, config_dir).await? else {
        bail!(
            "agent credentials are missing; configure hub-url/token or provision {}",
            credentials_path(config, config_dir).display(),
        );
    };
    connect_with_credentials(config, config_dir, &credentials, register_connection).await
}

async fn connect_with_credentials(
    config: &Config,
    config_dir: &Path,
    credentials: &ResolvedCredentials,
    register_connection: bool,
) -> anyhow::Result<WebsocketConnection> {
    let identity = load_device_identity(config, config_dir).await?;
    let connection = nexigon_client::ClientBuilder::new(
        credentials
            .hub_url
            .parse()
            .context("cannot parse hub URL")?,
        ClientToken::DeploymentToken(credentials.token.clone()),
    )
    .with_identity(Some(identity.client_identity))
    .with_device_fingerprint(Some(identity.fingerprint))
    .with_register_connection(register_connection)
    .dangerous_with_disable_tls(config.dangerous_disable_tls.unwrap_or(false))
    .connect()
    .await
    .context("cannot connect to Nexigon Hub")?;
    Ok(connection)
}

pub(crate) async fn load_device_identity(
    config: &Config,
    config_dir: &Path,
) -> anyhow::Result<DeviceIdentity> {
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
    Ok(DeviceIdentity {
        client_identity: identity,
        fingerprint,
    })
}

async fn resolve_credentials(
    config: &Config,
    config_dir: &Path,
) -> anyhow::Result<Option<ResolvedCredentials>> {
    match (&config.hub_url, &config.token) {
        (Some(hub_url), Some(token)) => {
            return Ok(Some(ResolvedCredentials {
                hub_url: hub_url.clone(),
                token: token.clone(),
            }));
        }
        (Some(_), None) | (None, Some(_)) => {
            bail!("agent config must set both hub-url and token, or neither");
        }
        (None, None) => {}
    }

    let path = credentials_path(config, config_dir);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let credentials: AgentCredentials =
        serde_json::from_str(&raw).with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(Some(credentials.resolved()))
}

fn provisioning_enabled(config: &Config) -> bool {
    config
        .provisioning
        .as_ref()
        .and_then(|p| p.enabled)
        .unwrap_or(false)
}

async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
}

pub(crate) async fn run_agent(
    config_path: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
    ready: Option<oneshot::Sender<DeviceId>>,
) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });

    let (config, config_dir) = load_config(&config_path).await?;
    let credentials = match resolve_credentials(&config, &config_dir).await? {
        Some(credentials) => credentials,
        None => {
            if !provisioning_enabled(&config) {
                bail!(
                    "agent credentials are missing and provisioning is disabled; expected {}",
                    credentials_path(&config, &config_dir).display(),
                );
            }
            let identity = load_device_identity(&config, &config_dir).await?;
            let credentials = provisioning::serve_until_paired(
                &config,
                &config_dir,
                &identity.fingerprint,
                shutdown_rx.clone(),
            )
            .await?;
            credentials.resolved()
        }
    };
    let connection = connect_with_credentials(&config, &config_dir, &credentials, true).await?;
    run_with_connection(
        config,
        &config_dir,
        connection,
        shutdown_signal(shutdown_rx),
        ready,
    )
    .await
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
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        run_agent(config_path, shutdown, Some(ready_tx)).await
    });
    AgentHandle {
        ready: ready_rx,
        join,
        shutdown: shutdown_tx,
    }
}
