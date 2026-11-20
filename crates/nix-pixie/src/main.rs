//! nix-pixie: provision NixOS machines from an empty disk to a running system.
//!
//! One binary with three roles; see `docs/design.md`.

mod agent;
mod controller;
mod discovery;
mod identity;
mod inventory;
mod mdns;
mod metadata;
mod net;
mod probe;
mod push;
mod state;
mod store;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Controller commands (run as a normal user).
    Controller {
        /// Controller identity key [default: $XDG_STATE_HOME/nix-pixie/identity].
        #[arg(long, global = true)]
        identity: Option<PathBuf>,
        #[command(subcommand)]
        command: ControllerCommand,
    },
    /// Run the installer-side agent (as root, inside the installer image).
    Agent {
        /// Directory for status files (code, endpoint id, paired controller).
        #[arg(long, default_value = "/run/nix-pixie")]
        state_dir: PathBuf,
        /// UDP port for the iroh endpoint.
        #[arg(long, default_value_t = nix_pixie_proto::DEFAULT_PORT)]
        port: u16,
        /// Pair using the code shown by a controller (`nix-pixie controller
        /// serve`) instead of showing a code here. The controller finds this
        /// machine and connects to it.
        #[arg(long, value_name = "CODE")]
        controller_code: Option<String>,
    },
    /// Serve DHCP and PXE for the installer image (needs root).
    Netboot,
}

#[derive(Debug, Subcommand)]
enum ControllerCommand {
    /// Print this controller's endpoint id (creates the identity if needed).
    Id,
    /// Pair with the target showing CODE; prints the paired target as JSON.
    Pair {
        /// The code shown on the target, e.g. 42-acid-acorn-acre.
        #[arg(long)]
        code: String,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// How to find targets.
        #[arg(long, value_enum, default_value_t = discovery::Method::Both)]
        discovery: discovery::Method,
        /// Flake whose `pixie` output describes your machines. Used to match
        /// the target to a machine and to write its facter report.
        #[arg(long)]
        flake: Option<String>,
        /// Install the target as this machine (default: the one whose hints
        /// match).
        #[arg(long, requires = "flake")]
        machine: Option<String>,
        /// Approve the target without asking.
        #[arg(long)]
        yes: bool,
        /// After approval, copy this store path and its closure into the
        /// target's disk (repeatable).
        #[arg(long, value_name = "STORE_PATH")]
        push: Vec<String>,
        /// Where the target's filesystems are mounted.
        #[arg(long, default_value = "/mnt", requires = "push")]
        store_root: String,
    },
    /// Show a controller code for targets to type in; prints one JSON line
    /// per paired target.
    Serve {
        /// Also write the current code to this file (mode 0600).
        #[arg(long)]
        code_file: Option<PathBuf>,
        /// Exit after pairing this many targets.
        #[arg(long)]
        max_targets: Option<usize>,
        /// How to find targets.
        #[arg(long, value_enum, default_value_t = discovery::Method::Both)]
        discovery: discovery::Method,
    },
}

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

    match Cli::parse().command {
        Command::Controller { identity, command } => {
            let identity = match identity {
                Some(path) => path,
                None => identity::default_path()?,
            };
            match command {
                ControllerCommand::Id => controller::print_id(&identity),
                ControllerCommand::Pair {
                    code,
                    timeout,
                    discovery,
                    flake,
                    machine,
                    yes,
                    push,
                    store_root,
                } => {
                    controller::pair(controller::PairArgs {
                        identity,
                        code,
                        timeout: Duration::from_secs(timeout),
                        method: discovery,
                        flake,
                        machine,
                        yes,
                        push,
                        store_root,
                    })
                    .await
                }
                ControllerCommand::Serve {
                    code_file,
                    max_targets,
                    discovery,
                } => {
                    controller::serve(controller::ServeArgs {
                        identity,
                        code_file,
                        max_targets,
                        method: discovery,
                    })
                    .await
                }
            }
        }
        Command::Agent {
            state_dir,
            port,
            controller_code,
        } => {
            agent::run(agent::AgentArgs {
                state_dir,
                port,
                controller_code,
            })
            .await
        }
        Command::Netboot => bail!("netboot is not implemented yet"),
    }
}

/// Resolves on Ctrl-C or SIGTERM (systemd stop).
pub async fn shutdown_signal() {
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            tracing::warn!("can't handle SIGTERM, only Ctrl-C: {e}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
