//! Nexigon Hub API client.

use std::sync::Arc;

use futures::Stream;
use futures::StreamExt;
use rustls::pki_types::pem::PemObject;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::error;
use url::Url;

use nexigon_ids::Id;
use nexigon_ids::ids::DeploymentToken;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_ids::ids::UserToken;
use nexigon_multiplex::Connection;
use nexigon_multiplex::ConnectionError;
use nexigon_multiplex::ConnectionEvent;
use nexigon_multiplex::ConnectionRef;

use crate::websocket::WebSocketTransport;

mod websocket;

/// Install Rustls crypto provider.
pub fn install_crypto_provider() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
}

/// Client mTLS identity.
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

    /// Disable TLS.
    pub fn dangerous_with_disable_tls(mut self, disable_tls: bool) -> Self {
        self.disable_tls = disable_tls;
        self
    }

    /// Disable TLS.
    pub fn dangerous_set_disable_tls(&mut self, disable_tls: bool) {
        self.disable_tls = disable_tls;
    }

    /// Connect to the Nexigon Hub server.
    pub async fn connect(&self) -> Result<WebsocketConnection, ClientError> {
        let mut ws_url = self.hub_url.clone();
        match ws_url.scheme() {
            "https" => ws_url.set_scheme("wss").unwrap(),
            "http" => ws_url.set_scheme("ws").unwrap(),
            _ => todo!("handle invalid URL scheme"),
        }
        ws_url.set_path("/api/v1/connect/ws");
        let connector = if self.disable_tls {
            tokio_tungstenite::Connector::Plain
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            // FIXME: We ignore any errors that occur while loading the certificates.
            for cert in rustls_native_certs::load_native_certs().certs {
                root_store.add(cert).unwrap();
            }
            let client_builder = rustls::ClientConfig::builder().with_root_certificates(root_store);
            let client_config = if let Some(identity) = &self.identity {
                client_builder.with_client_auth_cert(
                    vec![identity.certificate_der.clone()],
                    identity.private_key_der.clone_key(),
                )?
            } else {
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
        if let Some(identity) = &self.identity {
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
    pub fn spawn(mut self) {
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
        });
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
