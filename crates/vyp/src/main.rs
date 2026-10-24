pub mod accelerator;
mod build;
mod cache;
mod cli;
mod config;
mod env;
mod lock;
mod python;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "vyp", version, about = "Transitive-conflict-aware Python dependency resolver")]
struct Cli {
    #[command(subcommand)]
    command: cli::Commands,
}

fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    cli::run(cli.command)
}
