//! Nexigon Hub API client.

use std::future::Future;
use std::sync::Arc;

use futures::Stream;
use futures::StreamExt;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
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
use x509_cert::der::Decode;

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
///
/// Idempotent: subsequent calls after the first are no-ops, so libraries that
/// embed the agent (e.g. an in-process test host) can call this without
/// caring whether someone else already installed a provider.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Client mTLS identity.
#[derive(Debug)]
pub struct ClientIdentity {
    /// Client certificate in PEM format.
    certificate_pem: String,
    /// Client certificate chain in leaf-to-root DER order.
    certificate_chain_der: Vec<CertificateDer<'static>>,
    /// Client private key in DER format.
    private_key_der: PrivateKeyDer<'static>,
}

impl ClientIdentity {
    /// Create a new [`ClientIdentity`] with the given PEM-encoded certificate and private
    /// key.
    pub fn from_pem(certificate_pem: &str, private_key_pem: &str) -> Result<Self, InvalidPemError> {
        let certificate_chain_der = CertificateDer::pem_slice_iter(certificate_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(InvalidPemError::CertificatePem)?;
        if certificate_chain_der.is_empty() {
            return Err(InvalidPemError::EmptyCertificateChain);
        }
        for (index, certificate) in certificate_chain_der.iter().enumerate() {
            x509_cert::Certificate::from_der(certificate.as_ref()).map_err(|source| {
                InvalidPemError::InvalidCertificate {
                    index: index + 1,
                    source,
                }
            })?;
        }

        let mut private_keys = PrivateKeyDer::pem_slice_iter(private_key_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(InvalidPemError::PrivateKeyPem)?;
        if private_keys.len() != 1 {
            return Err(InvalidPemError::PrivateKeyCount(private_keys.len()));
        }
        let private_key_der = private_keys
            .pop()
            .ok_or(InvalidPemError::PrivateKeyCount(0))?;

        install_crypto_provider();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .ok_or(InvalidPemError::CryptoProviderUnavailable)?;
        rustls::sign::CertifiedKey::from_der(
            certificate_chain_der.clone(),
            private_key_der.clone_key(),
            provider,
        )
        .map_err(InvalidPemError::InvalidPrivateKey)?;

        Ok(Self {
            certificate_pem: certificate_pem.to_owned(),
            certificate_chain_der,
            private_key_der,
        })
    }
}

/// Invalid PEM error.
#[derive(Debug, Error)]
pub enum InvalidPemError {
    /// A certificate PEM block could not be decoded.
    #[error("cannot parse the client certificate PEM chain: {0}")]
    CertificatePem(#[source] rustls::pki_types::pem::Error),
    /// No certificate was provided.
    #[error("the client certificate PEM contains no certificates")]
    EmptyCertificateChain,
    /// A decoded certificate is not valid X.509 DER.
    #[error("client certificate #{index} is malformed: {source}")]
    InvalidCertificate {
        /// One-based position in the supplied chain.
        index: usize,
        /// Certificate parsing error.
        #[source]
        source: x509_cert::der::Error,
    },
    /// A private-key PEM block could not be decoded.
    #[error("cannot parse the client private-key PEM: {0}")]
    PrivateKeyPem(#[source] rustls::pki_types::pem::Error),
    /// The input did not contain exactly one supported private key.
    #[error("expected exactly one supported client private key, found {0}")]
    PrivateKeyCount(usize),
    /// The private key is unsupported, invalid, or does not match the leaf certificate.
    #[error(
        "the client private key is unsupported, invalid, or does not match the leaf certificate: {0}"
    )]
    InvalidPrivateKey(#[source] rustls::Error),
    /// No rustls cryptography provider is available.
    #[error("no rustls cryptography provider is available to validate the client identity")]
    CryptoProviderUnavailable,
}

/// Load and validate the operating system's native TLS trust roots.
fn load_native_root_store() -> Result<rustls::RootCertStore, ClientError> {
    let native = rustls_native_certs::load_native_certs();
    root_store_from_native_certificates(
        native.certs,
        native.errors.into_iter().map(|error| error.to_string()),
    )
}

/// Build a trust store from a native certificate-loader result.
fn root_store_from_native_certificates(
    certificates: Vec<CertificateDer<'static>>,
    load_errors: impl IntoIterator<Item = String>,
) -> Result<rustls::RootCertStore, ClientError> {
    let load_errors = load_errors.into_iter().collect::<Vec<_>>();
    if !load_errors.is_empty() {
        return Err(ClientError::NativeRootLoad(load_errors.join("; ")));
    }
    if certificates.is_empty() {
        return Err(ClientError::NoNativeRoots);
    }

    let mut root_store = rustls::RootCertStore::empty();
    for (index, certificate) in certificates.into_iter().enumerate() {
        root_store.add(certificate).map_err(|source| {
            ClientError::InvalidNativeRootCertificate {
                index: index + 1,
                source,
            }
        })?;
    }
    Ok(root_store)
}

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
    /// Allow credentials to be sent over a plaintext HTTP/WebSocket connection.
    allow_plaintext: bool,
    /// Accept an invalid TLS server certificate.
    accept_invalid_certificates: bool,
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
            allow_plaintext: false,
            accept_invalid_certificates: false,
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

    /// Allow credentials to be sent over a plaintext HTTP/WebSocket connection.
    ///
    /// This must only be used for explicitly trusted development networks. It does
    /// not affect certificate verification for TLS connections.
    pub fn dangerous_with_allow_plaintext(mut self, allow_plaintext: bool) -> Self {
        self.allow_plaintext = allow_plaintext;
        self
    }

    /// Set whether credentials may be sent over plaintext HTTP/WebSocket.
    pub fn dangerous_set_allow_plaintext(&mut self, allow_plaintext: bool) {
        self.allow_plaintext = allow_plaintext;
    }

    /// Accept an invalid TLS server certificate.
    ///
    /// The transport remains encrypted, but its peer is not authenticated. This
    /// option is independent from [`Self::dangerous_with_allow_plaintext`].
    pub fn dangerous_with_accept_invalid_certificates(
        mut self,
        accept_invalid_certificates: bool,
    ) -> Self {
        self.accept_invalid_certificates = accept_invalid_certificates;
        self
    }

    /// Set whether an invalid TLS server certificate should be accepted.
    pub fn dangerous_set_accept_invalid_certificates(&mut self, accept_invalid_certificates: bool) {
        self.accept_invalid_certificates = accept_invalid_certificates;
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
        install_crypto_provider();
        let (mut ws_url, plaintext) = self.websocket_url()?;
        ws_url.set_path("/api/v1/connect/ws");
        ws_url.set_query(None);
        ws_url.set_fragment(None);
        debug!(ws_url = %ws_url, "websocket URL");
        let connector = if plaintext {
            debug!("explicitly using a plaintext WebSocket connection");
            tokio_tungstenite::Connector::Plain
        } else {
            let root_store = if self.accept_invalid_certificates {
                // The explicit development verifier does not consult trust roots. Avoid making
                // that mode depend on a native store that may not exist in minimal containers.
                rustls::RootCertStore::empty()
            } else {
                load_native_root_store()?
            };
            let client_builder = rustls::ClientConfig::builder().with_root_certificates(root_store);
            let mut client_config = if let Some(identity) = &self.identity {
                debug!("TLS has been enabled, using client certificate");
                client_builder
                    .with_client_auth_cert(
                        identity.certificate_chain_der.clone(),
                        identity.private_key_der.clone_key(),
                    )
                    .map_err(|source| ClientError::InvalidClientIdentity { source })?
            } else {
                debug!("TLS has been enabled but no client certificate has been provided");
                client_builder.with_no_client_auth()
            };
            if self.accept_invalid_certificates {
                warn!("TLS server certificate verification has been disabled");
                let provider = rustls::crypto::CryptoProvider::get_default()
                    .ok_or(ClientError::CryptoProviderUnavailable)?
                    .clone();
                client_config
                    .dangerous()
                    .set_certificate_verifier(Arc::new(AcceptAnyServerCertificate(provider)));
            }
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
            && plaintext
        {
            warn!("sending the client certificate over an explicitly allowed plaintext channel");
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

    fn websocket_url(&self) -> Result<(Url, bool), ClientError> {
        let mut ws_url = self.hub_url.clone();
        match ws_url.scheme() {
            "https" => {
                ws_url
                    .set_scheme("wss")
                    .map_err(|_| ClientError::UnsupportedUrlScheme("https".to_owned()))?;
                Ok((ws_url, false))
            }
            "http" if self.allow_plaintext => {
                ws_url
                    .set_scheme("ws")
                    .map_err(|_| ClientError::UnsupportedUrlScheme("http".to_owned()))?;
                Ok((ws_url, true))
            }
            "http" => Err(ClientError::PlaintextTransportDisabled),
            scheme => Err(ClientError::UnsupportedUrlScheme(scheme.to_owned())),
        }
    }
}

/// Certificate verifier for the explicit dangerous development option.
#[derive(Debug)]
struct AcceptAnyServerCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Client connect error.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The Hub URL uses an unsupported scheme.
    #[error("unsupported Hub URL scheme `{0}`; expected https")]
    UnsupportedUrlScheme(String),
    /// A plaintext Hub URL was used without an explicit opt-in.
    #[error(
        "refusing to send Hub credentials over plaintext HTTP; enable the explicit dangerous plaintext option only for trusted development networks"
    )]
    PlaintextTransportDisabled,
    /// Native TLS root certificates could not be loaded.
    #[error("failed to load native TLS root certificates: {0}")]
    NativeRootLoad(String),
    /// No native TLS roots were available.
    #[error("the operating system provided no native TLS root certificates")]
    NoNativeRoots,
    /// A native TLS root certificate could not be parsed by rustls.
    #[error("native TLS root certificate #{index} is invalid: {source}")]
    InvalidNativeRootCertificate {
        /// One-based position in the native certificate result.
        index: usize,
        /// Certificate parsing error.
        #[source]
        source: rustls::Error,
    },
    /// A client identity failed final rustls configuration validation.
    #[error("cannot configure the TLS client identity: {source}")]
    InvalidClientIdentity {
        /// Identity validation error.
        #[source]
        source: rustls::Error,
    },
    /// No rustls cryptography provider is available.
    #[error("no rustls cryptography provider is available for TLS configuration")]
    CryptoProviderUnavailable,
    /// Invalid TLS configuration.
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    /// Websocket error.
    #[error(transparent)]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
    /// Connection error.
    #[error(transparent)]
    Connection(Box<ConnectionError<WebSocketTransport<MaybeTlsStream<TcpStream>>>>),
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

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Ws(Box::new(error))
    }
}

impl From<ConnectionError<WebSocketTransport<MaybeTlsStream<TcpStream>>>> for ClientError {
    fn from(error: ConnectionError<WebSocketTransport<MaybeTlsStream<TcpStream>>>) -> Self {
        Self::Connection(Box::new(error))
    }
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

/// Common interface for executors that can run Nexigon Hub actions.
///
/// Implemented by both [`ClientExecutor`] (direct hub link) and
/// [`local::LocalExecutor`] (via the agent local API socket), so callers
/// can be written generically against whichever transport is in use.
pub trait Execute {
    /// Execute the given [`Action`] and return its result.
    fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> impl Future<Output = Result<Result<A::Output, ActionError>, ExecuteError>>;
}

impl Execute for ClientExecutor {
    async fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        ClientExecutor::execute(self, action).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexigon_ids::Generate;
    use nexigon_ids::Id;
    use nexigon_ids::ids::DeploymentToken;
    use nexigon_ids::ids::DeviceFingerprint;
    use nexigon_ids::ids::UserToken;
    use rcgen::BasicConstraints;
    use rcgen::CertificateParams;
    use rcgen::ExtendedKeyUsagePurpose;
    use rcgen::IsCa;
    use rcgen::KeyPair;
    use rcgen::KeyUsagePurpose;
    use rustls::RootCertStore;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::Request;
    use tokio_tungstenite::tungstenite::handshake::server::Response;
    use url::Url;

    use super::ClientBuilder;
    use super::ClientError;
    use super::ClientIdentity;
    use super::ClientToken;
    use super::InvalidPemError;
    use super::install_crypto_provider;
    use super::root_store_from_native_certificates;

    #[derive(Debug)]
    struct RecordedRequest {
        path: String,
        authorization: String,
        user_token: String,
        deployment_token: String,
        device_fingerprint: String,
        client_certificate: String,
    }

    fn builder(url: Url, token: UserToken) -> ClientBuilder {
        ClientBuilder::new(url, ClientToken::UserToken(token))
    }

    fn pem_fixture(label: &str, body: &str) -> String {
        format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
    }

    struct ClientCertificateChain {
        root: rcgen::Certificate,
        intermediate: rcgen::Certificate,
        leaf: rcgen::Certificate,
        leaf_key: KeyPair,
    }

    fn client_certificate_chain() -> ClientCertificateChain {
        let mut root_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        root_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let root_key = KeyPair::generate().unwrap();
        let root = root_params.self_signed(&root_key).unwrap();

        let mut intermediate_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        intermediate_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let intermediate_key = KeyPair::generate().unwrap();
        let intermediate = intermediate_params
            .signed_by(&intermediate_key, &root, &root_key)
            .unwrap();

        let mut leaf_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = leaf_params
            .signed_by(&leaf_key, &intermediate, &intermediate_key)
            .unwrap();

        ClientCertificateChain {
            root,
            intermediate,
            leaf,
            leaf_key,
        }
    }

    // The callback error type is fixed by tungstenite's handshake API.
    #[allow(clippy::result_large_err)]
    fn record_request(
        sender: oneshot::Sender<RecordedRequest>,
    ) -> impl FnOnce(
        &Request,
        Response,
    ) -> Result<
        Response,
        tokio_tungstenite::tungstenite::http::Response<Option<String>>,
    > {
        move |request, response| {
            let recorded = RecordedRequest {
                path: request.uri().to_string(),
                authorization: request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
                user_token: request
                    .headers()
                    .get("x-user-token")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
                deployment_token: request
                    .headers()
                    .get("x-deployment-token")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
                device_fingerprint: request
                    .headers()
                    .get("x-device-fingerprint")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
                client_certificate: request
                    .headers()
                    .get("x-client-cert")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            };
            let _ = sender.send(recorded);
            Ok(response)
        }
    }

    async fn plaintext_server() -> (
        std::net::SocketAddr,
        oneshot::Receiver<RecordedRequest>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = accept_hdr_async(stream, record_request(sender)).await;
        });
        (address, receiver, task)
    }

    async fn tls_server(
        record: bool,
    ) -> (
        std::net::SocketAddr,
        Option<oneshot::Receiver<RecordedRequest>>,
        tokio::task::JoinHandle<()>,
    ) {
        install_crypto_provider();
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    certificate.to_der(),
                )],
                PrivateKeyDer::from_pem_slice(private_key.as_bytes()).unwrap(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let Ok(stream) = acceptor.accept(stream).await else {
                return;
            };
            if record {
                let _ = accept_hdr_async(stream, record_request(sender)).await;
            } else {
                drop(sender);
            }
        });
        (address, record.then_some(receiver), task)
    }

    async fn mtls_server(
        client_root: CertificateDer<'static>,
    ) -> (
        std::net::SocketAddr,
        oneshot::Receiver<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        install_crypto_provider();
        let mut client_roots = RootCertStore::empty();
        client_roots.add(client_root).unwrap();
        let client_verifier = rustls::server::WebPkiClientVerifier::builder(client_roots.into())
            .build()
            .unwrap();
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let server_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(
                vec![CertificateDer::from(certificate.to_der())],
                PrivateKeyDer::from_pem_slice(private_key.as_bytes()).unwrap(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let accepted = async {
                let (stream, _) = listener.accept().await?;
                let stream = acceptor.accept(stream).await?;
                accept_async(stream)
                    .await
                    .map(|_| ())
                    .map_err(std::io::Error::other)
            }
            .await
            .is_ok();
            let _ = sender.send(accepted);
        });
        (address, receiver, task)
    }

    #[test]
    fn client_identity_accepts_a_valid_direct_self_signed_certificate() {
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let identity = ClientIdentity::from_pem(&certificate.to_pem(), &private_key).unwrap();

        assert_eq!(identity.certificate_chain_der.len(), 1);
        assert_eq!(
            identity.certificate_chain_der[0].as_ref(),
            certificate.to_der()
        );
    }

    #[test]
    fn client_identity_rejects_empty_and_malformed_certificates() {
        let (_, private_key) = nexigon_cert::generate_self_signed_certificate();
        assert!(matches!(
            ClientIdentity::from_pem("", &private_key),
            Err(InvalidPemError::EmptyCertificateChain)
        ));
        assert!(matches!(
            ClientIdentity::from_pem(
                "-----BEGIN CERTIFICATE-----\nnot-base64\n-----END CERTIFICATE-----\n",
                &private_key,
            ),
            Err(InvalidPemError::CertificatePem(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pem(
                "-----BEGIN CERTIFICATE-----\nAAECAw==\n-----END CERTIFICATE-----\n",
                &private_key,
            ),
            Err(InvalidPemError::InvalidCertificate { index: 1, .. })
        ));
    }

    #[test]
    fn client_identity_requires_exactly_one_well_formed_private_key() {
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let malformed_key = pem_fixture("PRIVATE KEY", "not-base64");
        let invalid_key = pem_fixture("PRIVATE KEY", "AAECAw==");
        let encrypted_key = pem_fixture("ENCRYPTED PRIVATE KEY", "AAECAw==");
        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), ""),
            Err(InvalidPemError::PrivateKeyCount(0))
        ));
        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), &malformed_key),
            Err(InvalidPemError::PrivateKeyPem(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), &invalid_key),
            Err(InvalidPemError::InvalidPrivateKey(_))
        ));
        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), &encrypted_key),
            Err(InvalidPemError::PrivateKeyCount(0))
        ));

        let multiple_keys = format!("{private_key}{private_key}");
        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), &multiple_keys),
            Err(InvalidPemError::PrivateKeyCount(2))
        ));
    }

    #[test]
    fn client_identity_rejects_a_private_key_for_another_certificate() {
        let (certificate, _) = nexigon_cert::generate_self_signed_certificate();
        let (_, other_private_key) = nexigon_cert::generate_self_signed_certificate();

        assert!(matches!(
            ClientIdentity::from_pem(&certificate.to_pem(), &other_private_key),
            Err(InvalidPemError::InvalidPrivateKey(_))
        ));
    }

    #[test]
    fn native_root_failures_are_contextual_errors_without_panics() {
        let load_error = root_store_from_native_certificates(
            Vec::new(),
            ["cannot read test trust store".to_owned()],
        )
        .unwrap_err();
        assert!(matches!(
            load_error,
            ClientError::NativeRootLoad(message) if message.contains("test trust store")
        ));

        let malformed = root_store_from_native_certificates(
            vec![CertificateDer::from(vec![0, 1, 2, 3])],
            std::iter::empty(),
        )
        .unwrap_err();
        assert!(matches!(
            malformed,
            ClientError::InvalidNativeRootCertificate { index: 1, .. }
        ));

        assert!(matches!(
            root_store_from_native_certificates(Vec::new(), std::iter::empty()).unwrap_err(),
            ClientError::NoNativeRoots
        ));

        let (certificate, _) = nexigon_cert::generate_self_signed_certificate();
        let roots = root_store_from_native_certificates(
            vec![CertificateDer::from(certificate.to_der())],
            std::iter::empty(),
        )
        .unwrap();
        assert_eq!(roots.len(), 1);
    }

    #[tokio::test]
    async fn complete_leaf_and_intermediate_chain_is_presented_for_mtls() {
        let chain = client_certificate_chain();
        let certificate_pem = format!("{}{}", chain.leaf.pem(), chain.intermediate.pem());
        let identity =
            ClientIdentity::from_pem(&certificate_pem, &chain.leaf_key.serialize_pem()).unwrap();
        assert_eq!(identity.certificate_chain_der.len(), 2);
        assert_eq!(
            identity.certificate_chain_der[0].as_ref(),
            chain.leaf.der().as_ref()
        );
        assert_eq!(
            identity.certificate_chain_der[1].as_ref(),
            chain.intermediate.der().as_ref()
        );

        let (address, accepted, server) =
            mtls_server(CertificateDer::from(chain.root.der().to_vec())).await;
        let connection = builder(
            Url::parse(&format!(
                "https://localhost:{address_port}",
                address_port = address.port()
            ))
            .unwrap(),
            UserToken::generate(),
        )
        .with_identity(Some(identity))
        .dangerous_with_accept_invalid_certificates(true)
        .connect()
        .await
        .unwrap();
        assert!(accepted.await.unwrap());
        drop(connection);
        server.await.unwrap();
    }

    #[test]
    fn secure_url_policy_is_explicit() {
        let token = UserToken::generate();
        let secure = builder(Url::parse("https://hub.example").unwrap(), token.clone());
        let (url, plaintext) = secure.websocket_url().unwrap();
        assert_eq!(url.scheme(), "wss");
        assert!(!plaintext);

        let plaintext = builder(Url::parse("http://hub.example").unwrap(), token.clone());
        assert!(matches!(
            plaintext.websocket_url(),
            Err(ClientError::PlaintextTransportDisabled)
        ));

        let explicit_plaintext = plaintext.dangerous_with_allow_plaintext(true);
        let (url, plaintext) = explicit_plaintext.websocket_url().unwrap();
        assert_eq!(url.scheme(), "ws");
        assert!(plaintext);

        let invalid = builder(Url::parse("ftp://hub.example").unwrap(), token);
        assert!(matches!(
            invalid.websocket_url(),
            Err(ClientError::UnsupportedUrlScheme(scheme)) if scheme == "ftp"
        ));
    }

    #[tokio::test]
    async fn plaintext_credentials_require_the_explicit_opt_in() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = DeploymentToken::generate();
        let fingerprint = DeviceFingerprint::from_data(b"plaintext-policy-test");
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let identity = ClientIdentity::from_pem(&certificate.to_pem(), &private_key).unwrap();
        let error = ClientBuilder::new(
            Url::parse(&format!("http://{address}")).unwrap(),
            ClientToken::DeploymentToken(token),
        )
        .with_identity(Some(identity))
        .with_device_fingerprint(Some(fingerprint))
        .connect()
        .await
        .unwrap_err();
        assert!(matches!(error, ClientError::PlaintextTransportDisabled));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "the client connected before rejecting the plaintext URL",
        );
    }

    #[tokio::test]
    async fn explicit_plaintext_connection_sends_credentials() {
        let (address, request, server) = plaintext_server().await;
        let token = UserToken::generate();
        let expected = token.stringify();
        let connection = builder(Url::parse(&format!("http://{address}")).unwrap(), token)
            .dangerous_with_allow_plaintext(true)
            .connect()
            .await
            .unwrap();
        let request = request.await.unwrap();
        assert_eq!(request.path, "/api/v1/connect/ws");
        assert!(request.authorization.ends_with(&expected));
        assert_eq!(request.user_token, expected);
        assert!(request.deployment_token.is_empty());
        assert!(request.device_fingerprint.is_empty());
        assert!(request.client_certificate.is_empty());
        drop(connection);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_plaintext_device_connection_sends_all_authentication_headers() {
        let (address, request, server) = plaintext_server().await;
        let token = DeploymentToken::generate();
        let expected_token = token.stringify();
        let fingerprint = DeviceFingerprint::from_data(b"explicit-plaintext-test");
        let expected_fingerprint = fingerprint.stringify();
        let (certificate, private_key) = nexigon_cert::generate_self_signed_certificate();
        let certificate_pem = certificate.to_pem();
        let identity = ClientIdentity::from_pem(&certificate_pem, &private_key).unwrap();
        let connection = ClientBuilder::new(
            Url::parse(&format!("http://{address}")).unwrap(),
            ClientToken::DeploymentToken(token),
        )
        .with_identity(Some(identity))
        .with_device_fingerprint(Some(fingerprint))
        .dangerous_with_allow_plaintext(true)
        .connect()
        .await
        .unwrap();
        let request = request.await.unwrap();
        assert!(request.authorization.ends_with(&expected_token));
        assert_eq!(request.deployment_token, expected_token);
        assert_eq!(request.device_fingerprint, expected_fingerprint);
        assert_eq!(
            request.client_certificate,
            urlencoding::encode_binary(certificate_pem.as_bytes())
        );
        assert!(request.user_token.is_empty());
        drop(connection);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_tls_certificate_requires_its_separate_opt_in() {
        let (address, _, server) = tls_server(false).await;
        let token = UserToken::generate();
        let result = builder(
            Url::parse(&format!(
                "https://localhost:{address_port}",
                address_port = address.port()
            ))
            .unwrap(),
            token,
        )
        .connect()
        .await;
        assert!(matches!(result, Err(ClientError::Ws(_))));
        server.await.unwrap();

        let (address, request, server) = tls_server(true).await;
        let token = UserToken::generate();
        let expected = token.stringify();
        let connection = builder(
            Url::parse(&format!(
                "https://localhost:{address_port}",
                address_port = address.port()
            ))
            .unwrap(),
            token,
        )
        .dangerous_with_accept_invalid_certificates(true)
        .connect()
        .await
        .unwrap();
        let request = request.unwrap().await.unwrap();
        assert!(request.authorization.ends_with(&expected));
        assert_eq!(request.user_token, expected);
        drop(connection);
        server.await.unwrap();
    }
}
