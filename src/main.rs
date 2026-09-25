mod cli;
mod config;
mod daemon;
mod managers;
mod web;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "yat-cli", version, about = "YAT Linux WireGuard client")]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Import a shared network package from the desktop UI
  Import {
    /// Path to the share.json file
    file: std::path::PathBuf,
  },
  /// Connect to Edge and start WireGuard (foreground)
  Start,
  /// Show current network status
  Status,
  /// Leave network and teardown WireGuard interface
  Leave,
  /// Show certificate information
  CertInfo,
  /// Run as daemon with web management UI
  Serve {
    /// Listen address for the web UI
    #[arg(short, long, default_value = "0.0.0.0:9090")]
    listen: String,
  },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  // Initialize tracing
  fmt()
    .with_env_filter(EnvFilter::from_default_env().add_directive("yat_cli=info".parse()?))
    .init();

  let cli = Cli::parse();

  match cli.command {
    Commands::Import { file } => cli::import::run(&file),
    Commands::Start => cli::start::run().await,
    Commands::Status => cli::status::run(),
    Commands::Leave => cli::leave::run().await,
    Commands::CertInfo => cli::cert_info::run(),
    Commands::Serve { listen } => cli::serve::run(&listen).await,
  }
}
