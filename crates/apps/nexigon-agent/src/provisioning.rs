//! Pairing-key provisioning for unpaired agents.
//!
//! This is intentionally not a general-purpose HTTP server. It implements the
//! small request shape needed by `curl --data '<pairing-key>' /pair`, redeems
//! the key against the configured hub endpoints, writes `credentials.json`,
//! returns a short JSON status response, and exits.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use jiff::Timestamp;
use nexigon_api::types::errors::ActionError;
use nexigon_api::types::projects::RedeemDevicePairingKeyAction;
use nexigon_ids::ids::DeviceFingerprint;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::AgentCredentials;
use crate::config::Config;
use crate::credentials_path;

const DEFAULT_BIND: &str = "0.0.0.0:6947";
const DEFAULT_ENDPOINTS: &[&str] = &["https://eu.nexigon.cloud", "https://us.nexigon.cloud"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 1024;

#[derive(Debug)]
struct ProvisioningRuntime {
    bind: String,
    endpoints: Vec<String>,
    device_name: Option<String>,
    credentials_path: PathBuf,
    dangerous_allow_plaintext: bool,
    dangerous_accept_invalid_certificates: bool,
}

impl ProvisioningRuntime {
    fn from_config(config: &Config, config_dir: &Path) -> Self {
        let provisioning = config.provisioning.as_ref();
        let endpoints = provisioning
            .and_then(|p| p.endpoints.clone())
            .filter(|endpoints| !endpoints.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINTS.iter().map(|e| (*e).to_owned()).collect());
        Self {
            bind: provisioning
                .and_then(|p| p.bind.clone())
                .unwrap_or_else(|| DEFAULT_BIND.to_owned()),
            endpoints,
            device_name: provisioning.and_then(|p| p.device_name.clone()),
            credentials_path: credentials_path(config, config_dir),
            dangerous_allow_plaintext: config.dangerous_allow_plaintext.unwrap_or(false),
            dangerous_accept_invalid_certificates: config
                .dangerous_accept_invalid_certificates
                .unwrap_or(false),
        }
    }
}

/// Serve the local provisioning endpoint until a pairing key is redeemed.
pub async fn serve_until_paired(
    config: &Config,
    config_dir: &Path,
    fingerprint: &DeviceFingerprint,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<AgentCredentials> {
    let runtime = ProvisioningRuntime::from_config(config, config_dir);
    if let Some(parent) = runtime.credentials_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let listener = TcpListener::bind(&runtime.bind)
        .await
        .with_context(|| format!("binding provisioning endpoint at {}", runtime.bind))?;
    info!(
        bind = %runtime.bind,
        credentials_path = %runtime.credentials_path.display(),
        "agent provisioning endpoint listening",
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                bail!("shutdown signaled before provisioning completed");
            }
            accepted = listener.accept() => {
                let (stream, peer) = accepted.context("accepting provisioning connection")?;
                debug!(%peer, "accepted provisioning connection");
                match handle_client(stream, &runtime, fingerprint).await {
                    Ok(Some(credentials)) => return Ok(credentials),
                    Ok(None) => {}
                    Err(e) => warn!("provisioning client error: {e:#}"),
                }
            }
        }
    }
}

async fn handle_client(
    mut stream: TcpStream,
    runtime: &ProvisioningRuntime,
    fingerprint: &DeviceFingerprint,
) -> anyhow::Result<Option<AgentCredentials>> {
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            write_error(&mut stream, error.status, &error.message).await?;
            return Ok(None);
        }
        Err(_) => {
            write_error(&mut stream, 408, "request timed out").await?;
            return Ok(None);
        }
    };

    if request.method == "GET" && request.path == "/" {
        write_json(
            &mut stream,
            200,
            &json!({
                "status": "ready",
                "endpoint": "/pair",
            }),
        )
        .await?;
        return Ok(None);
    }
    if request.method != "POST" || request.path != "/pair" {
        write_error(&mut stream, 404, "unknown provisioning endpoint").await?;
        return Ok(None);
    }

    let pairing_key = match parse_pairing_key(&request.body) {
        Ok(pairing_key) => pairing_key,
        Err(error) => {
            write_error(&mut stream, 400, &error).await?;
            return Ok(None);
        }
    };

    match redeem_pairing_key(&pairing_key, runtime, fingerprint).await {
        Ok((credentials, device_id)) => {
            store_credentials(&runtime.credentials_path, &credentials).await?;
            if let Err(error) = write_json(
                &mut stream,
                200,
                &json!({
                    "status": "paired",
                    "hubUrl": credentials.hub_url,
                    "deviceId": device_id.to_string(),
                }),
            )
            .await
            {
                warn!(
                    ?error,
                    "failed to write provisioning success response after storing credentials",
                );
            }
            Ok(Some(credentials))
        }
        Err(error) => {
            write_error(&mut stream, 400, &format!("{error:#}")).await?;
            Ok(None)
        }
    }
}

async fn redeem_pairing_key(
    pairing_key: &str,
    runtime: &ProvisioningRuntime,
    fingerprint: &DeviceFingerprint,
) -> anyhow::Result<(AgentCredentials, nexigon_ids::ids::DeviceId)> {
    let mut last_error = None;
    for endpoint in &runtime.endpoints {
        match redeem_pairing_key_at(endpoint, pairing_key, runtime, fingerprint).await {
            Ok(output) => {
                let credentials = AgentCredentials {
                    hub_url: endpoint.clone(),
                    deployment_token: output.token,
                    paired_at: Timestamp::now(),
                };
                return Ok((credentials, output.device_id));
            }
            Err(error) => {
                warn!(%endpoint, "pairing redemption failed: {error:#}");
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) => Err(error).context("all provisioning endpoints failed"),
        None => bail!("no provisioning endpoints configured"),
    }
}

async fn redeem_pairing_key_at(
    endpoint: &str,
    pairing_key: &str,
    runtime: &ProvisioningRuntime,
    fingerprint: &DeviceFingerprint,
) -> anyhow::Result<nexigon_api::types::projects::RedeemDevicePairingKeyOutput> {
    let mut url: reqwest::Url = endpoint.parse().context("cannot parse hub URL")?;
    validate_endpoint_transport(&url, runtime.dangerous_allow_plaintext)?;
    if url.scheme() == "http" {
        warn!(%url, "sending a pairing key over explicitly allowed plaintext HTTP");
    }
    let action_path = format!(
        "{}/api/v1/actions/invoke/projects_RedeemDevicePairingKey",
        url.path().trim_end_matches('/')
    );
    url.set_path(&action_path);
    url.set_query(None);
    url.set_fragment(None);

    let action = RedeemDevicePairingKeyAction::new(pairing_key.to_owned(), fingerprint.clone())
        .with_device_name(runtime.device_name.clone());
    let body = serde_json::to_vec(&action).context("serializing pairing redemption request")?;
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(runtime.dangerous_accept_invalid_certificates)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building provisioning HTTP client")?;
    let response = client
        .post(url.clone())
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .with_context(|| format!("cannot call pairing redemption at {url}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("reading pairing redemption response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<ActionError>(&body) {
            bail!("pairing redemption failed: {}", error.message);
        }
        bail!(
            "pairing redemption failed with HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    serde_json::from_slice(&body).context("cannot parse pairing redemption response")
}

fn validate_endpoint_transport(url: &reqwest::Url, allow_plaintext: bool) -> anyhow::Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_plaintext => Ok(()),
        "http" => bail!(
            "refusing to send a pairing key over plaintext HTTP; enable dangerous-allow-plaintext only for a trusted development network"
        ),
        scheme => bail!("unsupported provisioning Hub URL scheme `{scheme}`; expected https"),
    }
}

async fn store_credentials(path: &Path, credentials: &AgentCredentials) -> anyhow::Result<()> {
    let mut data = serde_json::to_vec_pretty(credentials).context("serializing credentials")?;
    data.push(b'\n');
    nexigon_common::secure_file::write_private(path, data, true)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    info!(path = %path.display(), "stored provisioned agent credentials");
    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, HttpError> {
    let mut buf = Vec::new();
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(HttpError::new(431, "request headers too large"));
        }
        let mut chunk = [0; 1024];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| HttpError::new(400, "failed to read request"))?;
        if n == 0 {
            return Err(HttpError::new(400, "connection closed before headers"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_bytes = &buf[..header_end];
    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|_| HttpError::new(400, "request headers must be UTF-8"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpError::new(400, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing request method"))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing request path"))?
        .to_owned();
    let version = request_parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing HTTP version"))?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::new(505, "unsupported HTTP version"));
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::new(400, "malformed request header"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    if headers.contains_key("transfer-encoding") {
        return Err(HttpError::new(400, "transfer-encoding is not supported"));
    }
    let body_len = match headers.get("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| HttpError::new(400, "invalid content-length"))?,
        None => 0,
    };
    if body_len > MAX_BODY_BYTES {
        return Err(HttpError::new(413, "request body too large"));
    }

    let body_start = header_end + 4;
    let mut body = buf.get(body_start..).unwrap_or_default().to_vec();
    if body.len() > body_len {
        body.truncate(body_len);
    }
    while body.len() < body_len {
        let mut chunk = vec![0; body_len - body.len()];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| HttpError::new(400, "failed to read request body"))?;
        if n == 0 {
            return Err(HttpError::new(400, "connection closed before request body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }

    Ok(HttpRequest { method, path, body })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingJson {
    pairing_key: String,
}

fn parse_pairing_key(body: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(body)
        .map_err(|_| "request body must be UTF-8".to_owned())?
        .trim();
    if text.is_empty() {
        return Err("pairing key is required".to_owned());
    }
    if text.starts_with('{') {
        let parsed: PairingJson =
            serde_json::from_str(text).map_err(|_| "invalid pairing JSON".to_owned())?;
        let pairing_key = parsed.pairing_key.trim();
        if pairing_key.is_empty() {
            return Err("pairing key is required".to_owned());
        }
        Ok(pairing_key.to_owned())
    } else {
        Ok(text.to_owned())
    }
}

async fn write_error(stream: &mut TcpStream, status: u16, message: &str) -> anyhow::Result<()> {
    write_json(
        stream,
        status,
        &json!({
            "status": "error",
            "error": message,
        }),
    )
    .await
}

async fn write_json<T: Serialize>(
    stream: &mut TcpStream,
    status: u16,
    value: &T,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value).expect("JSON response serialization is infallible");
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pairing_key;
    use super::validate_endpoint_transport;

    #[test]
    fn parses_plain_pairing_key() {
        assert_eq!(parse_pairing_key(b"ABCD-123456\n").unwrap(), "ABCD-123456");
    }

    #[test]
    fn parses_json_pairing_key() {
        assert_eq!(
            parse_pairing_key(br#"{"pairingKey":"ABCD-123456"}"#).unwrap(),
            "ABCD-123456",
        );
    }

    #[test]
    fn provisioning_requires_secure_transport_by_default() {
        let https = reqwest::Url::parse("https://hub.example").unwrap();
        assert!(validate_endpoint_transport(&https, false).is_ok());

        let http = reqwest::Url::parse("http://hub.example").unwrap();
        assert!(validate_endpoint_transport(&http, false).is_err());
        assert!(validate_endpoint_transport(&http, true).is_ok());

        let ftp = reqwest::Url::parse("ftp://hub.example").unwrap();
        assert!(validate_endpoint_transport(&ftp, true).is_err());
    }
}
