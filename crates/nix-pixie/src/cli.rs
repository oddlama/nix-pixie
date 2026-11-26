//! The command line: argument definitions and dispatch to the roles.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::net::Relays;
use crate::{agent, console, control, controller, discovery, events, identity, install, target};

/// The command line.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Which relay servers to meet at (`--relay`, `--no-relay`). A target that
/// shows a code waits at a relay, where a controller on another network
/// finds it without the target having to be reachable; the relay also
/// carries their (encrypted) traffic until they have found a direct path.
#[derive(Debug, clap::Args)]
pub struct RelayArgs {
    /// Meet at this relay server (an `iroh-relay`; repeatable) instead of
    /// the public ones run by n0, the makers of iroh. Controller and target
    /// need the same.
    #[arg(long = "relay", value_name = "URL")]
    relays: Vec<String>,
    /// Use no relay: pair on the local network, or with `--target`.
    #[arg(long, conflicts_with = "relays")]
    no_relay: bool,
}

impl RelayArgs {
    fn relays(&self) -> Result<Relays> {
        Relays::from_args(&self.relays, self.no_relay)
    }
}

/// Top-level commands, one per role.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Controller commands (run as a normal user).
    Controller {
        /// Controller identity key [default: $XDG_STATE_HOME/nix-pixie/identity].
        #[arg(long, global = true)]
        identity: Option<PathBuf>,
        /// Also write progress as JSON lines to FILE (a file, a FIFO or
        /// /dev/fd/N), for a program that drives the controller. The exit
        /// codes say how far an install got; see the README.
        #[arg(long, global = true, value_name = "FILE")]
        events: Option<PathBuf>,
        /// Neither substitute nor fetch anything: everything the install
        /// needs is in the local store already.
        #[arg(long, global = true)]
        offline: bool,
        #[command(subcommand)]
        command: Box<ControllerCommand>,
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
        /// machine and connects to it, on the local network or with
        /// `--target`: relays aren't used.
        #[arg(long, value_name = "CODE")]
        controller_code: Option<String>,
        /// Like --controller-code, from a file, e.g. a systemd credential
        /// set on the kernel command line: unattended, with no screen.
        #[arg(long, value_name = "FILE", conflicts_with = "controller_code")]
        controller_code_file: Option<PathBuf>,
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// On the target (as root): how its relay stands, or which relays the
    /// running agent uses from now on, e.g. your own iroh-relay. The agent
    /// shows a new code with them. Lasts until the target reboots.
    Relay {
        /// Relay URLs, `off` (none: only this network), `n0` (n0's public
        /// relays) or `default` (as the agent was started). Without any,
        /// how the relay stands.
        setting: Vec<String>,
        /// The agent's state directory.
        #[arg(long, default_value = "/run/nix-pixie")]
        state_dir: PathBuf,
    },
    /// The installer's tty1: the agent's code, relay and messages, and a
    /// prompt for commands like `relay URL`.
    Console {
        /// The agent's state directory.
        #[arg(long, default_value = "/run/nix-pixie")]
        state_dir: PathBuf,
        /// Line by line instead of a screen, for a serial port: changes as
        /// they come, commands as lines.
        #[arg(long)]
        plain: bool,
        /// Like --plain, as JSON lines, for a program on the other end.
        #[arg(long, conflicts_with = "plain")]
        json: bool,
    },
    /// Serve DHCP and PXE for the installer image (needs root).
    Netboot,
    /// For hooks: the target of the install the hook belongs to.
    Target {
        #[command(subcommand)]
        command: TargetCommand,
    },
}

/// What hooks can do on the target.
#[derive(Debug, Subcommand)]
pub enum TargetCommand {
    /// Run PROGRAM, a file in the Nix store (e.g. ${pkgs.sbctl}/bin/sbctl),
    /// on the target: the controller copies it there with its closure and
    /// runs it without a shell, input or the installer's environment.
    /// Exits like it, with its output on stdout and stderr. For hooks, as
    /// `"$PIXIE" target exec -- PROGRAM ARGS...`, from `approve` until
    /// `finish`.
    Exec {
        /// Inside the installed system (as nixos-enter does), from hooks
        /// after `install`.
        #[arg(long)]
        chroot: bool,
        /// Give the program this command's stdin (up to 512 KiB), e.g. a
        /// key, rather than none. It is read to the end first.
        #[arg(long)]
        stdin: bool,
        /// The program, then its arguments.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

/// `nix-pixie controller install`.
#[derive(Debug, Args)]
pub struct InstallCli {
    /// The code shown on the target, e.g. 482913-acid-acorn-acre. Asked
    /// for on the terminal if left out, which keeps it out of the
    /// shell's history.
    #[arg(long)]
    code: Option<String>,
    /// Pair with a target waiting for this controller's code (three
    /// words, from `nix-pixie controller new-code`), which was handed to
    /// it some other way, e.g. on the kernel command line of an
    /// installer booted over USB. For targets without a screen.
    #[arg(long, value_name = "FILE", conflicts_with = "code")]
    controller_code_file: Option<PathBuf>,
    /// Flake with the `pixie` output and the configurations.
    #[arg(long, default_value = ".")]
    flake: String,
    /// Which of many devices installed from one machine this is, e.g. a
    /// serial number or an id from an inventory: `{id}` in the machine's
    /// paths (hostKey.publicKeyPath, facter.reportPath), and `id` for
    /// hooks. A hook can give it instead, in `$PIXIE_OUT/id`.
    #[arg(long)]
    id: Option<String>,
    /// A binary cache the target copies the system from first (e.g. one
    /// on its own network, faster than through the controller; it needs
    /// a network to it); the controller sends what is missing.
    /// Repeatable.
    #[arg(long = "substituter", value_name = "URL")]
    substituters: Vec<String>,
    /// A key whose signatures the target trusts for the substituters,
    /// e.g. cache.example.org-1:AbC...=. Repeatable.
    #[arg(
        long = "trusted-public-key",
        value_name = "KEY",
        requires = "substituters"
    )]
    trusted_public_keys: Vec<String>,
    /// Don't compress what goes to the target: for links faster than
    /// zstd on this machine, or contents that don't compress.
    #[arg(long)]
    no_compression: bool,
    /// How to check that the installed system came up, instead of the
    /// machine's bootCheck: ask its SSH server, leave it to the hooks
    /// after `boot`, or not at all.
    #[arg(long, value_enum)]
    boot_check: Option<crate::metadata::BootCheck>,
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
    /// How often to ask the installed system for its host key while
    /// waiting, in seconds. Pressing Enter asks right away.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    boot_check_interval: u64,
    /// Give up pairing after this many seconds.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    /// How to find targets.
    #[arg(long, value_enum, default_value_t = discovery::Method::Both)]
    discovery: discovery::Method,
    /// Instead of searching the local network, probe this host (an IP
    /// address or a name; repeatable), e.g. a server on the internet
    /// running `nix run github:oddlama/nix-pixie#agent`. Needs UDP 47011
    /// and 47012 to it. On a direct link, a link-local address with its
    /// interface: `fe80::2%usb0`.
    #[arg(long = "target", value_name = "HOST")]
    targets: Vec<String>,
    #[command(flatten)]
    relay: RelayArgs,
}

impl InstallCli {
    /// What `install` runs with.
    fn into_args(self, identity: PathBuf) -> Result<install::InstallArgs> {
        let Self {
            code,
            controller_code_file,
            id,
            substituters,
            trusted_public_keys,
            no_compression,
            boot_check,
            flake,
            machine,
            disks,
            yes,
            wipe,
            then,
            boot_timeout,
            boot_check_interval,
            timeout,
            discovery,
            targets,
            relay,
        } = self;
        if no_compression {
            crate::push::no_compression();
        }
        Ok(install::InstallArgs {
            identity,
            code,
            controller_code_file,
            id,
            caches: install::Caches {
                from: substituters,
                keys: trusted_public_keys,
            },
            boot_check,
            timeout: Duration::from_secs(timeout),
            method: discovery,
            flake,
            machine,
            disks,
            yes,
            wipe,
            then,
            boot_timeout: Duration::from_secs(boot_timeout),
            boot_check_interval: Duration::from_secs(boot_check_interval),
            targets,
            relays: relay.relays()?,
        })
    }
}

/// What the controller can do.
#[derive(Debug, Subcommand)]
pub enum ControllerCommand {
    /// Print this controller's endpoint id (creates the identity if needed).
    Id,
    /// Print a new controller's code, for a target that gets it some other
    /// way than by typing (`agent --controller-code-file`) and for
    /// `--controller-code-file` here. Use each code once.
    NewCode,
    /// Pair with the target showing CODE; prints the paired target as JSON.
    Pair {
        /// The code shown on the target, e.g. 482913-acid-acorn-acre. Asked
        /// for on the terminal if left out, which keeps it out of the
        /// shell's history.
        #[arg(long)]
        code: Option<String>,
        /// Pair with a target waiting for this controller's code (three
        /// words, from `nix-pixie controller new-code`), which was handed to
        /// it some other way, e.g. on the kernel command line of an
        /// installer booted over USB. For targets without a screen.
        #[arg(long, value_name = "FILE", conflicts_with = "code")]
        controller_code_file: Option<PathBuf>,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// How to find targets.
        #[arg(long, value_enum, default_value_t = discovery::Method::Both)]
        discovery: discovery::Method,
        /// Instead of searching the local network, probe this host (an IP
        /// address or a name; repeatable), e.g. a server on the internet
        /// running `nix run github:oddlama/nix-pixie#agent`. Needs UDP 47011
        /// and 47012 to it. On a direct link, a link-local address with its
        /// interface: `fe80::2%usb0`.
        #[arg(long = "target", value_name = "HOST")]
        targets: Vec<String>,
        #[command(flatten)]
        relay: RelayArgs,
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
    Install(Box<InstallCli>),
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
        /// Instead of searching the local network, probe this host (an IP
        /// address or a name; repeatable), e.g. a server on the internet
        /// running `nix run github:oddlama/nix-pixie#agent`. Needs UDP 47011
        /// and 47012 to it. On a direct link, a link-local address with its
        /// interface: `fe80::2%usb0`.
        #[arg(long = "target", value_name = "HOST")]
        targets: Vec<String>,
    },
}

/// Runs the command `cli` describes, and says how to exit.
///
/// # Errors
///
/// Whatever the command fails with.
pub async fn run(cli: Cli) -> Result<ExitCode> {
    let result = match cli.command {
        Command::Controller {
            identity,
            events,
            offline,
            command,
        } => {
            if let Some(path) = &events {
                events::open(path)?;
            }
            if offline {
                crate::nix::offline();
            }
            let identity = match identity {
                Some(path) => path,
                None => identity::default_path()?,
            };
            run_controller(identity, *command).await
        }
        Command::Agent {
            state_dir,
            port,
            controller_code,
            controller_code_file,
            relay,
        } => {
            let controller_code = match controller_code_file {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading the code from {}", path.display()))?
                        .trim()
                        .to_string(),
                ),
                None => controller_code,
            };
            agent::run(agent::AgentArgs {
                state_dir,
                port,
                controller_code,
                relays: relay.relays()?,
            })
            .await
        }
        Command::Relay { setting, state_dir } => relay(&state_dir, &setting).await,
        Command::Console {
            state_dir,
            plain,
            json,
        } => match (plain, json) {
            (false, false) => console::run(state_dir).await,
            (_, true) => console::run_lines(state_dir, console::Lines::Json).await,
            (true, false) => console::run_lines(state_dir, console::Lines::Plain).await,
        },
        Command::Netboot => bail!("netboot is not implemented yet"),
        Command::Target {
            command:
                TargetCommand::Exec {
                    chroot,
                    stdin,
                    command,
                },
        } => {
            let mut command = command.into_iter();
            let program = command.next().unwrap_or_default();
            return target::exec(program, command.collect(), chroot, stdin).await;
        }
    };
    result.map(|()| ExitCode::SUCCESS)
}

/// `nix-pixie relay`: changes the agent's relays, or says how they stand.
async fn relay(state_dir: &Path, words: &[String]) -> Result<()> {
    if words.is_empty() {
        let status = control::watch(state_dir).await?.next().await?;
        println!(
            "{}",
            status
                .relay
                .as_deref()
                .unwrap_or("No relays: only controllers on this network find this machine.")
        );
        if let Some(shown) = status.code {
            println!("Pairing code: {} ({})", shown.code, shown.command);
        }
        return Ok(());
    }
    let done = control::set_relays(state_dir, control::Setting::parse(words)?).await?;
    println!("{done}");
    Ok(())
}

async fn run_controller(identity: PathBuf, command: ControllerCommand) -> Result<()> {
    match command {
        ControllerCommand::Id => controller::print_id(&identity),
        ControllerCommand::NewCode => {
            println!("{}", nix_pixie_proto::code::Code::generate(None)?);
            Ok(())
        }
        ControllerCommand::Pair {
            code,
            controller_code_file,
            timeout,
            discovery,
            flake,
            machine,
            yes,
            push,
            store_root,
            targets,
            relay,
        } => {
            controller::pair(controller::PairArgs {
                identity,
                code,
                controller_code_file,
                timeout: Duration::from_secs(timeout),
                method: discovery,
                flake,
                machine,
                yes,
                push,
                store_root,
                targets,
                relays: relay.relays()?,
            })
            .await
        }
        ControllerCommand::Install(install) => install::install(install.into_args(identity)?).await,
        ControllerCommand::Serve {
            code_file,
            max_targets,
            discovery,
            targets,
        } => {
            controller::serve(controller::ServeArgs {
                identity,
                code_file,
                max_targets,
                method: discovery,
                targets,
            })
            .await
        }
    }
}
