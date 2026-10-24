mod cli;
mod client_owner;
mod config;
mod daemon;
mod gather;
mod managers;
mod web;

use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(
  name = "yat-client",
  version,
  about = "YAT WireGuard client for Linux servers"
)]
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
    /// Listen address for the web UI (env: YAT_LISTEN)
    #[arg(short, long, env = "YAT_LISTEN", default_value = "0.0.0.0:9909")]
    listen: String,
  },
}

fn main() -> anyhow::Result<()> {
  let cli = Cli::parse();
  if matches!(cli.command, Commands::Start | Commands::Serve { .. }) {
    yat_wireguard::platform::ensure_platform_kernel_support()?;
  }
  // Block before creating any runtime threads. The owner signal thread only
  // notifies the same supervisor; native cleanup stays on the original runtime.
  if matches!(
    cli.command,
    Commands::Start | Commands::Serve { .. } | Commands::Leave | Commands::Import { .. }
  ) {
    unsafe {
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);
      let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
      anyhow::ensure!(error == 0, "Cannot block shutdown signals: {error}");
    }
  }

  // Build the tokio runtime manually (equivalent to #[tokio::main]).
  // This is required so that the signal blocking above takes effect before
  // any tokio worker threads are spawned.
  let rt = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?;

  rt.block_on(async {
    // Initialize tracing
    fmt()
      .with_env_filter(EnvFilter::from_default_env().add_directive("yat_cli=info".parse()?))
      .init();

    match cli.command {
      Commands::Import { file } => cli::import::run(&file).await,
      Commands::Start => cli::start::run().await,
      Commands::Status => cli::status::run(),
      Commands::Leave => cli::leave::run().await,
      Commands::CertInfo => cli::cert_info::run(),
      Commands::Serve { listen } => cli::serve::run(&listen).await,
    }
  })
}
