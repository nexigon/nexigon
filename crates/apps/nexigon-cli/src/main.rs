use std::path::PathBuf;

use clap::Parser;

use nexigon_client::ClientToken;

use crate::config::Config;

pub mod config;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON").apply(&args.logging);
    let config =
        toml::from_str::<Config>(&tokio::fs::read_to_string(&args.config).await.unwrap()).unwrap();
    nexigon_client::install_crypto_provider();
    let connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().unwrap(),
        ClientToken::UserToken(config.token.clone()),
    )
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
