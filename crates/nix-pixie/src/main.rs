//! The `nix-pixie` executable: the composition root. It sets up the console,
//! installs the tracing subscriber writing through it, and hands over to
//! [`nix_pixie::cli::run`].

use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use nix_pixie::cli::{self, Cli};
use nix_pixie::ui::{self, Mode};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Log timestamps on a live terminal: the time since the start, which is
/// what matters while watching one run, in a fixed-width column.
struct Elapsed(Instant);

impl FormatTime for Elapsed {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{:>6}", ui::duration(self.0.elapsed()))
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let console = ui::init();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn,nix_pixie=info".into());
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // No colour codes in pipes, files or the journal.
        .with_ansi(console.colors)
        .with_writer(ui::log_writer);
    match console.mode {
        // Module paths and dates are for logs read later, not on a terminal.
        Mode::Live => logs
            .with_target(false)
            .with_timer(Elapsed(Instant::now()))
            .init(),
        Mode::Plain => logs.init(),
    }
    let result = cli::run(Cli::parse()).await;
    ui::finish();
    result
}
