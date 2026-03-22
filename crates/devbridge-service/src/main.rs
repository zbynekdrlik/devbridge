mod runtime;
mod service;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use devbridge_core::Config;

#[derive(Parser, Debug)]
#[command(name = "devbridge-service", about = "DevBridge print bridge service")]
struct Cli {
    /// Path to configuration file.
    #[arg(short, long, default_value = "config/default.toml")]
    config: PathBuf,

    /// Override the operating mode from the config file.
    #[arg(short, long)]
    mode: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = Config::load(&cli.config)?;

    if let Some(mode) = cli.mode {
        config.general.mode = mode;
    }

    // If running as a Windows service, dispatch to the service handler.
    if std::env::args().any(|a| a == "--service") {
        return service::run_as_service();
    }

    // Otherwise, run in foreground mode.
    let config_path = cli.config.canonicalize().ok();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(runtime::run(config, config_path))
}
