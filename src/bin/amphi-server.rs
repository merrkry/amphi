use std::path::PathBuf;

use clap::Parser;

/// Polls GitHub, evaluates flakes and dispatches their builds to workers.
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
    let config = amphi::server::config::Config::load(&args.config)?;
    amphi::server::run(config).await
}
