//! The `nix-pixie` executable: the composition root. It installs the tracing
//! subscriber and hands over to [`nix_pixie::cli::run`].

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;
use nix_pixie::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,nix_pixie=info".into()),
        )
        // No colour codes in the journal.
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
    cli::run(Cli::parse()).await
}
