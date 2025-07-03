use std::path::PathBuf;

use clap::Parser;

use nexigon_client::ClientIdentity;
use nexigon_client::ClientToken;
use nexigon_ids::ids::DeviceFingerprint;

use crate::config::Config;

pub mod config;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON").apply(&args.logging);
    let config_path = args.config.canonicalize().unwrap();
    let config_dir = config_path.parent().unwrap();
    let config =
        toml::from_str::<Config>(&tokio::fs::read_to_string(&args.config).await.unwrap()).unwrap();
    nexigon_client::install_crypto_provider();
    let cert = tokio::fs::read_to_string(config_dir.join(config.ssl_cert.unwrap()))
        .await
        .unwrap();
    let key = tokio::fs::read_to_string(config_dir.join(config.ssl_key.unwrap()))
        .await
        .unwrap();
    let identity = ClientIdentity::from_pem(&cert, &key).unwrap();
    let connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().unwrap(),
        ClientToken::DeploymentToken(config.token.clone()),
    )
    .with_identity(Some(identity))
    .with_device_fingerprint(Some(DeviceFingerprint::from_data(b"xyz")))
    .dangerous_with_disable_tls(config.dangerous_disable_tls.unwrap_or(false))
    .connect()
    .await;
    println!("{connection:?}")
}

/// CLI arguments.
#[derive(Debug, Parser)]
pub struct Args {
    /// Logging arguments.
    #[clap(flatten)]
    logging: si_observability::clap4::LoggingArgs,
    /// Configuration file.
    #[clap(long)]
    config: PathBuf,
}
