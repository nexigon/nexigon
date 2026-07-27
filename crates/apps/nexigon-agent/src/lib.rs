//! Nexigon Agent library.
//!
//! In addition to telemetry, provisioning, and interactive remote commands, the agent
//! polls the Hub for asynchronous device-operation **command** steps. Before dispatching
//! a command it records the attempt in a durable local ledger. A completed result is
//! retried without re-executing the command if reporting fails; an attempt interrupted
//! after dispatch is failed conservatively because replaying an external side effect is
//! unsafe.
//!
//! The operation protocol also defines durable `DeviceTask` work, but this agent does not
//! currently poll for or execute task steps.

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
mod operation_ledger;
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
    let file_name = config_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config path has no filename"))?;
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let config_dir = parent
        .canonicalize()
        .context("cannot canonicalize config directory")?;
    let config_path = config_dir.join(file_name);
    let raw_config = nexigon_common::secure_file::read_private(&config_path)
        .await
        .context("agent config must be a private non-symlink regular file")?;
    let raw_config = String::from_utf8(raw_config).context("agent config is not valid UTF-8")?;
    let config = toml::from_str::<Config>(&raw_config).context("cannot parse config")?;
    Ok((Arc::new(config), config_dir))
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
    connect_with_identity(config, credentials, identity, register_connection).await
}

async fn connect_with_identity(
    config: &Config,
    credentials: &ResolvedCredentials,
    identity: DeviceIdentity,
    register_connection: bool,
) -> anyhow::Result<WebsocketConnection> {
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
    .dangerous_with_allow_plaintext(config.dangerous_allow_plaintext.unwrap_or(false))
    .dangerous_with_accept_invalid_certificates(
        config
            .dangerous_accept_invalid_certificates
            .unwrap_or(false),
    )
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
    let cert_exists = regular_file_exists(&cert_path, "SSL certificate").await?;
    let key_exists = private_file_exists(&key_path, "SSL private key").await?;
    match (cert_exists, key_exists) {
        (false, true) => bail!("found SSL key but certificate is missing"),
        (true, false) => bail!("found SSL certificate but private key is missing"),
        (false, false) => {
            info!(?cert_path, "generating SSL certificate and key");
            generate_device_identity_files(
                &cert_path,
                &key_path,
                config.ssl_cert.is_none(),
                config.ssl_key.is_none(),
            )
            .await?;
        }
        (true, true) => {}
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
    let cert = nexigon_common::secure_file::read_regular(&cert_path)
        .await
        .context("cannot read certificate")?;
    let key = nexigon_common::secure_file::read_private(&key_path)
        .await
        .context("cannot read private key")?;
    let cert = String::from_utf8(cert).context("certificate is not valid UTF-8")?;
    let key = String::from_utf8(key).context("private key is not valid UTF-8")?;
    let identity = ClientIdentity::from_pem(&cert, &key).context("cannot parse identity")?;
    Ok(DeviceIdentity {
        client_identity: identity,
        fingerprint,
    })
}

async fn generate_device_identity_files(
    cert_path: &Path,
    key_path: &Path,
    protect_cert_parent: bool,
    protect_key_parent: bool,
) -> anyhow::Result<()> {
    let (certificate, key) = nexigon_cert::generate_self_signed_certificate();
    nexigon_common::secure_file::write_private(key_path, key, protect_key_parent)
        .await
        .context("cannot create private key securely")?;
    nexigon_common::secure_file::write_public(cert_path, certificate.to_pem(), protect_cert_parent)
        .await
        .context("cannot create certificate atomically")?;
    Ok(())
}

async fn regular_file_exists(path: &Path, description: &str) -> anyhow::Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => {
            nexigon_common::secure_file::validate_regular(path)
                .await
                .with_context(|| format!("{description} must be a non-symlink regular file"))?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

async fn private_file_exists(path: &Path, description: &str) -> anyhow::Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => {
            // The subsequent descriptor-based read attempts to repair an insecure
            // mode. Existing agents must still start when that chmod is not allowed.
            nexigon_common::secure_file::validate_regular(path)
                .await
                .with_context(|| format!("{description} must be a non-symlink regular file"))?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
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
    let raw = match nexigon_common::secure_file::read_private(&path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "credentials must be a private regular file: {}",
                    path.display()
                )
            });
        }
    };
    let raw = String::from_utf8(raw)
        .with_context(|| format!("credentials are not valid UTF-8: {}", path.display()))?;
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
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });

    let result = async {
        let (config, config_dir) = load_config(&config_path).await?;
        let (credentials, identity) = match resolve_credentials(&config, &config_dir).await? {
            Some(credentials) => {
                let identity = load_device_identity(&config, &config_dir).await?;
                (credentials, identity)
            }
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
                (credentials.resolved(), identity)
            }
        };
        // Network connection setup is cancellation-safe. Identity loading is deliberately
        // completed before this race because the fingerprint subprocess does not yet have the
        // bounded/cancellation-aware contract tracked by P1-12.
        let connection = tokio::select! {
            biased;
            () = shutdown_signal(shutdown_rx.clone()) => return Ok(()),
            connection = connect_with_identity(&config, &credentials, identity, true) => connection?,
        };
        // Once connected, run_with_connection owns cooperative cancellation and awaited task
        // cleanup. Do not race and drop the whole session from this outer layer.
        run_with_connection(
            config,
            &config_dir,
            connection,
            shutdown_signal(shutdown_rx.clone()),
            ready,
        )
        .await
    }
    .await;

    shutdown_task.abort();
    if let Err(error) = shutdown_task.await
        && !error.is_cancelled()
    {
        return Err(error).context("shutdown signal task panicked");
    }
    result
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

#[cfg(all(test, unix))]
mod credential_file_tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::generate_device_identity_files;
    use super::load_config;
    use super::private_file_exists;

    #[tokio::test]
    async fn generated_private_key_is_atomic_and_mode_0600() {
        let root = tempdir().unwrap();
        let identity_dir = root.path().join("identity");
        let cert_path = identity_dir.join("cert.pem");
        let key_path = identity_dir.join("key.pem");

        generate_device_identity_files(&cert_path, &key_path, true, true)
            .await
            .unwrap();

        assert_eq!(
            std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600,
        );
        assert_eq!(
            std::fs::metadata(&identity_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
        );
        assert!(private_file_exists(&key_path, "test key").await.unwrap());
    }

    #[tokio::test]
    async fn existing_insecure_private_key_is_accepted_but_symlink_is_rejected() {
        let root = tempdir().unwrap();
        let key_path = root.path().join("key.pem");
        std::fs::write(&key_path, b"key").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_file_exists(&key_path, "test key").await.unwrap());

        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link_path = root.path().join("key-link.pem");
        std::os::unix::fs::symlink(&key_path, &link_path).unwrap();
        assert!(private_file_exists(&link_path, "test key").await.is_err());
    }

    #[tokio::test]
    async fn config_read_repairs_mode_but_never_follows_a_symlink() {
        let root = tempdir().unwrap();
        let config_path = root.path().join("agent.toml");
        std::fs::write(&config_path, "fingerprint-script = \"fingerprint\"\n").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        load_config(&config_path).await.unwrap();
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );

        let link_path = root.path().join("agent-link.toml");
        std::os::unix::fs::symlink(&config_path, &link_path).unwrap();
        assert!(load_config(&link_path).await.is_err());
    }
}
