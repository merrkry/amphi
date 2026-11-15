use std::path::PathBuf;

use clap::Parser;

/// Builds derivations the Amphi server hands out and publishes them to niks3.
#[derive(Parser)]
struct Args {
    /// TOML configuration file.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    amphi::init_tracing();
    let config = amphi::worker::Config::load(&args.config)?;
    amphi::worker::run(config).await
}
