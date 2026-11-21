//! The command line: argument definitions and dispatch to the roles.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::{agent, controller, discovery, identity, install};

/// The command line.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands, one per role.
#[derive(Debug, Subcommand)]
pub enum Command {
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

/// What the controller can do.
#[derive(Debug, Subcommand)]
pub enum ControllerCommand {
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
    /// Pair with the target showing CODE and install a machine of FLAKE onto
    /// it: choose disks, build, format with disko, and send the system.
    /// Prints the result as JSON.
    Install {
        /// The code shown on the target, e.g. 42-acid-acorn-acre.
        #[arg(long)]
        code: String,
        /// Flake with the `pixie` output and the configurations.
        #[arg(long, default_value = ".")]
        flake: String,
        /// Install this machine (default: the one whose hints match).
        #[arg(long)]
        machine: Option<String>,
        /// Put ROLE (a disko disk) onto DISK: a kernel name like nvme0n1,
        /// a /dev/disk/by-id name, or a serial number (repeatable).
        #[arg(long = "disk", value_name = "ROLE=DISK")]
        disks: Vec<String>,
        /// Approve the target and format empty disks without asking.
        #[arg(long)]
        yes: bool,
        /// Erase disks that hold data without asking.
        #[arg(long)]
        wipe: bool,
        /// What the target does once the system is installed. After a
        /// reboot or power-off, the controller waits for the installed
        /// system's SSH server and checks its host key.
        #[arg(long, value_enum, default_value_t = install::Then::Reboot)]
        then: install::Then,
        /// How long to wait for the installed system, in seconds.
        #[arg(long, default_value_t = 600)]
        boot_timeout: u64,
        /// Give up pairing after this many seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// How to find targets.
        #[arg(long, value_enum, default_value_t = discovery::Method::Both)]
        discovery: discovery::Method,
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

/// Runs the command `cli` describes.
///
/// # Errors
///
/// Whatever the command fails with.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
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
                ControllerCommand::Install {
                    code,
                    flake,
                    machine,
                    disks,
                    yes,
                    wipe,
                    then,
                    boot_timeout,
                    timeout,
                    discovery,
                } => {
                    install::install(install::InstallArgs {
                        identity,
                        code,
                        timeout: Duration::from_secs(timeout),
                        method: discovery,
                        flake,
                        machine,
                        disks,
                        yes,
                        wipe,
                        then,
                        boot_timeout: Duration::from_secs(boot_timeout),
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
