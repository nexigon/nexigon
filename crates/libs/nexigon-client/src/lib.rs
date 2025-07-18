//! Nexigon Hub API client.

use std::sync::Arc;

use futures::Stream;
use futures::StreamExt;
use rustls::pki_types::pem::PemObject;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use url::Url;

use nexigon_api::Action;
use nexigon_api::types::errors::ActionError;
use nexigon_ids::Id;
use nexigon_ids::ids::DeploymentToken;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_ids::ids::UserToken;
use nexigon_multiplex::Channel;
use nexigon_multiplex::Connection;
use nexigon_multiplex::ConnectionError;
use nexigon_multiplex::ConnectionEvent;
use nexigon_multiplex::ConnectionRef;
use nexigon_rpc::ExecuteError;

use crate::websocket::WebSocketTransport;

mod websocket;

/// Install Rustls crypto provider.
pub fn install_crypto_provider() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
}

/// Client mTLS identity.
#[derive(Debug)]
pub struct ClientIdentity {
    /// Client certificate in PEM format.
    certificate_pem: String,
    /// Client certificate in DER format.
    certificate_der: rustls::pki_types::CertificateDer<'static>,
    /// Client private key in DER format.
    private_key_der: rustls::pki_types::PrivateKeyDer<'static>,
}

impl ClientIdentity {
    /// Create a new [`ClientIdentity`] with the given PEM-encoded certificate and private
    /// key.
    pub fn from_pem(certificate_pem: &str, private_key_pem: &str) -> Result<Self, InvalidPemError> {
        Ok(Self {
            certificate_pem: certificate_pem.to_owned(),
            certificate_der: rustls::pki_types::CertificateDer::from_pem_slice(
                certificate_pem.as_bytes(),
            )
            .map_err(InvalidPemError)?,
            private_key_der: rustls::pki_types::PrivateKeyDer::from_pem_slice(
                private_key_pem.as_bytes(),
            )
            .map_err(InvalidPemError)?,
        })
    }
}

/// Invalid PEM error.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct InvalidPemError(rustls::pki_types::pem::Error);

/// Client token to use for authentication.
#[derive(Debug, Clone)]
pub enum ClientToken {
    /// Deployment token.
    DeploymentToken(DeploymentToken),
    /// User token.
    UserToken(UserToken),
}

impl ClientToken {
    /// Return the token as a string.
    pub fn stringify(&self) -> String {
        match self {
            Self::DeploymentToken(token) => token.stringify(),
            Self::UserToken(token) => token.stringify(),
        }
    }
}

/// Client builder.
#[derive(Debug)]
pub struct ClientBuilder {
    /// Server URL.
    hub_url: Url,
    /// Token to use for authentication.
    token: ClientToken,
    /// Optional client identity.
    identity: Option<ClientIdentity>,
    /// Optional device fingerprint.
    device_fingerprint: Option<DeviceFingerprint>,
    /// Disable TLS.
    disable_tls: bool,
    /// Indicates whether the connection should be registered.
    register_connection: bool,
}

impl ClientBuilder {
    /// Create a new [`ClientBuilder`] with the given server URL.
    pub fn new(hub_url: Url, token: ClientToken) -> Self {
        Self {
            hub_url,
            token,
            identity: None,
            device_fingerprint: None,
            disable_tls: false,
            register_connection: true,
        }
    }

    /// Set the client identity.
    pub fn with_identity(mut self, identity: Option<ClientIdentity>) -> Self {
        self.identity = identity;
        self
    }

    /// Set the client identity.
    pub fn set_identity(&mut self, identity: Option<ClientIdentity>) {
        self.identity = identity;
    }

    /// Set the device fingerprint.
    pub fn with_device_fingerprint(
        mut self,
        device_fingerprint: Option<DeviceFingerprint>,
    ) -> Self {
        self.device_fingerprint = device_fingerprint;
        self
    }

    /// Set the device fingerprint.
    pub fn set_device_fingerprint(&mut self, device_fingerprint: Option<DeviceFingerprint>) {
        self.device_fingerprint = device_fingerprint;
    }

    /// Set whether TLS should be disabled.
    pub fn dangerous_with_disable_tls(mut self, disable_tls: bool) -> Self {
        self.disable_tls = disable_tls;
        self
    }

    /// Set whether TLS should be disabled.
    pub fn dangerous_set_disable_tls(&mut self, disable_tls: bool) {
        self.disable_tls = disable_tls;
    }

    /// Set whether the connection should be registered.
    pub fn with_register_connection(mut self, register_connection: bool) -> Self {
        self.register_connection = register_connection;
        self
    }

    /// Set whether the connection should be registered.
    pub fn set_register_connection(&mut self, register_connection: bool) {
        self.register_connection = register_connection;
    }

    /// Connect to the Nexigon Hub server.
    #[tracing::instrument(level = tracing::Level::DEBUG, skip_all)]
    pub async fn connect(&self) -> Result<WebsocketConnection, ClientError> {
        info!("establishing websocket connection to Nexigon Hub");
        let mut ws_url = self.hub_url.clone();
        match ws_url.scheme() {
            "https" => ws_url.set_scheme("wss").unwrap(),
            "http" => ws_url.set_scheme("ws").unwrap(),
            _ => todo!("handle invalid URL scheme"),
        }
        ws_url.set_path("/api/v1/connect/ws");
        debug!(ws_url = %ws_url, "websocket URL");
        let connector = if self.disable_tls {
            debug!("TLS has been disabled, using plain connector");
            tokio_tungstenite::Connector::Plain
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            // FIXME: We ignore any errors that occur while loading the certificates.
            for cert in rustls_native_certs::load_native_certs().certs {
                root_store.add(cert).unwrap();
            }
            let client_builder = rustls::ClientConfig::builder().with_root_certificates(root_store);
            let client_config = if let Some(identity) = &self.identity {
                debug!("TLS has been enabled, using client certificate");
                client_builder.with_client_auth_cert(
                    vec![identity.certificate_der.clone()],
                    identity.private_key_der.clone_key(),
                )?
            } else {
                debug!("TLS has been enabled but no client certificate has been provided");
                client_builder.with_no_client_auth()
            };
            tokio_tungstenite::Connector::Rustls(Arc::new(client_config))
        };
        let mut request = ws_url.into_client_request()?;
        request.headers_mut().append(
            "Authorization",
            format!("Bearer {}", self.token.stringify())
                .try_into()
                .unwrap(),
        );
        request.headers_mut().append(
            "X-Register-Connection",
            self.register_connection.to_string().try_into().unwrap(),
        );
        match &self.token {
            ClientToken::DeploymentToken(token) => {
                request
                    .headers_mut()
                    .append("X-Deployment-Token", token.stringify().try_into().unwrap());
            }
            ClientToken::UserToken(token) => {
                request
                    .headers_mut()
                    .append("X-User-Token", token.stringify().try_into().unwrap());
            }
        }
        if let Some(device_fingerprint) = &self.device_fingerprint {
            request.headers_mut().append(
                "X-Device-Fingerprint",
                device_fingerprint.stringify().try_into().unwrap(),
            );
        }
        if let Some(identity) = &self.identity
            && self.disable_tls
        {
            warn!("TLS has been disabled, sending client certificate in header");
            request.headers_mut().append(
                "X-Client-Cert",
                urlencoding::encode_binary(identity.certificate_pem.as_bytes())
                    .into_owned()
                    .try_into()
                    .unwrap(),
            );
        }
        let (socket, _) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector))
                .await?;
        let transport = WebSocketTransport::new(socket);
        let connection = Connection::new(transport);
        Ok(WebsocketConnection { connection })
    }
}

/// Client connect error.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Invalid TLS configuration.
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    /// Websocket error.
    #[error(transparent)]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    /// Connection error.
    #[error(transparent)]
    Connection(#[from] ConnectionError<WebSocketTransport<MaybeTlsStream<TcpStream>>>),
    /// IO error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Serialization.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// Open error.
    #[error(transparent)]
    Open(#[from] nexigon_multiplex::OpenError),
    /// Other error.
    #[error("{0}")]
    Other(String),
    /// Action error.
    #[error("action error: {}", _0.message)]
    ActionError(ActionError),
}

/// Websocket connection to a Nexigon Hub server.
///
/// This is a special type of [`nexigon_multiplex::Connection`].
#[derive(Debug)]
pub struct WebsocketConnection {
    /// Underling connection.
    connection: nexigon_multiplex::Connection<WebSocketTransport<MaybeTlsStream<TcpStream>>>,
}

impl WebsocketConnection {
    /// Return a reference to the underlying connection.
    pub fn make_ref(&self) -> ConnectionRef {
        self.connection.make_ref()
    }

    /// Spawn a new task polling the connection.
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(event) = self.connection.next().await {
                match event {
                    Ok(_) => { /* ignore all events */ }
                    Err(error) => {
                        error!("connection error: {error}");
                        break;
                    }
                }
            }
        })
    }
}

impl Stream for WebsocketConnection {
    type Item = Result<ConnectionEvent, ClientError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.connection.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(event))) => std::task::Poll::Ready(Some(Ok(event))),
            std::task::Poll::Ready(Some(Err(error))) => {
                std::task::Poll::Ready(Some(Err(error.into())))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Connect an executor via the given [`ConnectionRef`].
pub async fn connect_executor(
    connection: &mut ConnectionRef,
) -> Result<ClientExecutor, ClientError> {
    let channel = connection.open(b"executor").await?;
    Ok(ClientExecutor::new(channel))
}

/// Executor for executing [`Action`]s on the Nexigon Hub server.
#[derive(Debug)]
pub struct ClientExecutor {
    /// Channel for sending and receiving data.
    channel: Channel,
}

impl ClientExecutor {
    /// Construct a new [`ClientExecutor`] from the given [`Channel`].
    fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// Execute the given [`Action`] on the Nexigon Hub server.
    pub async fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        let (tx, rx) = self.channel.split_mut();
        nexigon_rpc::execute(&action, rx, tx).await
    }
}
