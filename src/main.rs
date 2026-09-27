mod cli;
mod config;
mod daemon;
mod gather;
mod managers;
mod web;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "yat-client", version, about = "YAT Linux WireGuard client")]
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
  // ── Pre-runtime signal blocking for `serve` ─────────────────────────
  // `#[tokio::main]` spawns worker threads BEFORE the function body runs.
  // Those workers inherit the process signal mask with SIGINT *unblocked*,
  // so the kernel delivers Ctrl+C to a tokio worker (default = terminate)
  // and the sigwait cleanup thread never fires.
  //
  // Fix: block SIGINT/SIGTERM *here*, before the tokio runtime is created.
  // All subsequently spawned threads (including tokio workers) inherit the
  // blocked mask, ensuring only the dedicated sigwait thread can catch the
  // signal and run the WireGuard cleanup.
  //
  // Only applied to `serve` — other commands use `tokio::signal::ctrl_c()`.
  let is_serve = std::env::args().any(|a| a == "serve");
  if is_serve {
    unsafe {
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);
      libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
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

    let cli = Cli::parse();

    match cli.command {
      Commands::Import { file } => cli::import::run(&file),
      Commands::Start => cli::start::run().await,
      Commands::Status => cli::status::run(),
      Commands::Leave => cli::leave::run().await,
      Commands::CertInfo => cli::cert_info::run(),
      Commands::Serve { listen } => cli::serve::run(&listen).await,
    }
  })
}
