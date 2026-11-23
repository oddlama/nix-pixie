//! `nix-pixie controller install`: from a paired target to formatted disks
//! with the system on them.
//!
//! Everything that can fail without touching the target's disks happens
//! first: evaluating the configuration, choosing the disks, recording them,
//! building. Then the operator confirms what gets erased, and only then does
//! the target format its disks and receive the system.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, VarInt};
use nix_pixie_proto::code::Code;
use nix_pixie_proto::ctrl::{
    ControllerMsg, DiskAssignment, KeyFile, MAX_TARGET_FRAME, Secret, Shutdown, StreamRequest,
    TargetMsg,
};
use nix_pixie_proto::frame::{read_frame, write_frame};
use nix_pixie_proto::inventory::Disk;
use tracing::{info, trace};
use zeroize::Zeroizing;

use crate::controller::{self, Approved, confirm, describe_disk};
use crate::discovery::{self, Discovery, Method, Peer};
use crate::hooks::{self, Step};
use crate::hostkey::{self, PublicKey};
use crate::identity;
use crate::metadata::{self, HostKeyMode, Machine, Metadata};
use crate::net::{self, Stream, sanitise};
use crate::plan::{self, Choice, Resolution};
use crate::preflight;
use crate::push;
use crate::secrets;
use crate::system::{Built, Configuration, Disko, Facts, Ssh};

/// For the agent's checks before formatting (a fresh lsblk).
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
/// Unmounting and exporting pools.
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);
/// How much of disko's and nixos-install's output to keep for showing a
/// failure.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
const OUTPUT_TAIL_LINES: usize = 40;

/// How `nix-pixie controller install` was invoked.
pub struct InstallArgs {
    pub identity: PathBuf,
    pub code: String,
    pub timeout: Duration,
    pub method: Method,
    /// Flake with the `pixie` output and the configurations.
    pub flake: String,
    /// Install the target as this machine of the flake.
    pub machine: Option<String>,
    /// `ROLE=DISK` choices.
    pub disks: Vec<String>,
    /// Approve the target and format empty disks without asking.
    pub yes: bool,
    /// Erase disks that hold data without asking.
    pub wipe: bool,
    /// What the target does once the system is installed.
    pub then: Then,
    /// How long to wait for the installed system's SSH server.
    pub boot_timeout: Duration,
    /// Probe these hosts instead of searching the local network.
    pub targets: Vec<String>,
}

/// What the target does once the system is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Then {
    /// Boot the installed system, then check its host key.
    Reboot,
    /// Power off (e.g. to remove the installer medium), then wait for the
    /// operator to power it on and check its host key.
    Poweroff,
    /// Keep the installer running with everything mounted, for inspection.
    Stay,
}

/// Pairs with the target showing the code, and installs the machine onto it.
pub async fn install(args: InstallArgs) -> Result<()> {
    let code = Code::parse(&args.code)?;
    let nameplate = code.nameplate().context(
        "target codes start with the number shown on the target, e.g. 42-acid-acorn-acre",
    )?;
    let requested = parse_disk_args(&args.disks)?;
    let metadata = metadata::load(&args.flake).await?;
    // Check what we can before pairing: a mistake shouldn't cost the code.
    let programs = if let Some(name) = &args.machine {
        let machine = machine(&metadata, &args.flake, name)?;
        check_push(name, machine)?;
        if let Some(report) = missing_facter_report(&args.flake, machine) {
            // A configuration that reads its facter report can't evaluate
            // before the first one arrives (after approval); check it then.
            eprintln!(
                "Not evaluating nixosConfigurations.{} before pairing: its facter report \
                 ({report}) isn't there yet.",
                machine.configuration
            );
        } else {
            let disko = disko_of(&Configuration::new(&args.flake, &machine.configuration)?).await?;
            plan::check_requested(&disko, machine.disks.file.is_some(), &requested)?;
            let has_hooks = !machine.hooks.is_empty();
            secrets::check_available(&secrets::needs(&disko.encryption)?, has_hooks)?;
        }
        if machine.disks.file.is_some() {
            repo_root(&args.flake)?;
        }
        hooks::schedule(&machine.hooks)?;
        Some(hooks::resolve(&args.flake, &machine.hooks).await?)
    } else {
        None
    };

    let targets = discovery::resolve_targets(&args.targets).await?;
    let endpoint = net::bind(identity::load_or_create(&args.identity)?, 0).await?;
    let mut discovery = Discovery::start(args.method, targets).await?;
    let result = tokio::select! {
        r = async {
            let (conn, mut stream, peer) = controller::pair_with_target(
                &endpoint, &mut discovery, &code, nameplate, args.timeout,
            )
            .await?;
            let result = session(
                &mut stream, &conn, &peer, &metadata, &args, requested, programs,
            )
            .await;
            // Closing is what lets the target shut down.
            net::finish(&mut stream).await;
            conn.close(VarInt::from_u32(0), b"done");
            result
        } => r,
        () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
    };
    discovery.shutdown();
    endpoint.close().await;
    let Installed {
        mut report,
        check,
        mut hooks,
    } = result?;
    let done = match check {
        Some(check) => tokio::select! {
            r = async {
                verify(check, &args, &mut report).await?;
                hooks.after(Step::Boot).await?;
                no_files_left(&mut hooks, Step::Boot)
            } => r,
            () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
        },
        None => Ok(()),
    };
    report.insert("hooks".into(), hooks.report());
    println!("{}", serde_json::Value::Object(report));
    done
}

/// Files that hooks after `step` wrote can't go anywhere: the target is gone.
fn no_files_left(hooks: &mut hooks::Runner, step: Step) -> Result<()> {
    if hooks.take_files().is_empty() {
        Ok(())
    } else {
        bail!("hooks after `{step}` can't put files into the installed system any more")
    }
}

/// What the session leaves for after the connection is closed.
struct Installed {
    report: serde_json::Map<String, serde_json::Value>,
    /// How to check the installed system's host key, if it can be.
    check: Option<Check>,
    /// For the hooks after `boot`.
    hooks: hooks::Runner,
}

/// Where and what to ask the installed system's SSH server.
struct Check {
    addrs: Vec<std::net::SocketAddr>,
    port: u16,
    /// Pull or push mode: the key the install put there.
    expected: Option<PublicKey>,
    /// The installer's own key, which may still answer for a moment.
    live: Option<PublicKey>,
    machine: String,
    /// The first boot may wait for a disk passphrase on the console.
    encrypted: bool,
}

/// Waits for the installed system and compares its host key; records the
/// result in `report`.
async fn verify(
    check: Check,
    args: &InstallArgs,
    report: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let wait = args.boot_timeout.as_secs();
    if args.then == Then::Poweroff {
        eprintln!(
            "\n{} is powered off. Remove the installer medium if needed and power it on; \
             waiting up to {wait} s for it.",
            check.machine
        );
    } else {
        eprintln!("\nWaiting up to {wait} s for {} to boot.", check.machine);
    }
    if check.encrypted {
        eprintln!(
            "Its disks are encrypted: it may ask for the passphrase on its console while booting; \
             type it there."
        );
    }
    let result = hostkey::wait_for(
        &check.addrs,
        check.port,
        check.expected.as_ref(),
        check.live.as_ref(),
        args.boot_timeout,
    )
    .await;
    let entry = match &result {
        Ok(seen) => {
            let verified = check.expected.is_some();
            if verified {
                eprintln!(
                    "{} is up, with the host key the install put there.",
                    check.machine
                );
            } else {
                eprintln!(
                    "{} is up. Its host key is new, so it can't be checked: {}",
                    check.machine, seen.key
                );
            }
            serde_json::json!({
                "verified": verified,
                "seen": seen.key.to_string(),
                "address": seen.addr.to_string(),
            })
        }
        Err(e) => serde_json::json!({ "verified": false, "error": format!("{e:#}") }),
    };
    report.insert("hostKey".into(), entry);
    result.map(|_| ())
}

/// The machine's facter report path, if it wants one and the repository
/// doesn't have it yet (a first install).
fn missing_facter_report<'a>(flake: &str, machine: &'a Machine) -> Option<&'a str> {
    let report = machine.facter.report_path.as_deref()?;
    let root = metadata::local_flake_dir(flake)?;
    (!root.join(report).exists()).then_some(report)
}

/// Push mode needs a hook to provide the key.
fn check_push(name: &str, machine: &Machine) -> Result<()> {
    if machine.host_key.mode == HostKeyMode::Push && machine.hooks.is_empty() {
        bail!(
            "{name} has hostKey.mode = \"push\" but no hooks; a hook that runs before \
             `host-key` provides the key in $PIXIE_OUT/host-key"
        );
    }
    Ok(())
}

/// Everything after pairing: first deciding and building, which touches
/// nothing on the target's disks, then applying.
async fn session(
    stream: &mut Stream,
    conn: &Connection,
    peer: &Peer,
    metadata: &Metadata,
    args: &InstallArgs,
    requested: BTreeMap<String, String>,
    programs: Option<BTreeMap<String, PathBuf>>,
) -> Result<Installed> {
    let plan = prepare(stream, conn, peer, metadata, args, requested, programs).await?;
    apply(stream, conn, peer, args, plan).await
}

/// Everything decided and built before anything is erased.
struct Plan {
    label: String,
    name: String,
    configuration: String,
    disks: Vec<Disk>,
    choices: Vec<Choice>,
    built: Built,
    disko: Disko,
    ssh: Ssh,
    /// Pull mode: the generated key, and where the installed system wants it.
    host_key: Option<(PublicKey, String)>,
    /// Passphrases disko reads from files while formatting.
    key_files: Vec<KeyFile>,
    hooks: hooks::Runner,
}

/// Approval, the facter report and host key, the disk plan, the build.
async fn prepare(
    stream: &mut Stream,
    conn: &Connection,
    peer: &Peer,
    metadata: &Metadata,
    args: &InstallArgs,
    mut requested: BTreeMap<String, String>,
    programs: Option<BTreeMap<String, PathBuf>>,
) -> Result<Plan> {
    let Approved {
        label,
        inventory,
        machine: name,
        ..
    } = controller::approve(
        stream,
        conn,
        peer,
        Some(metadata),
        args.machine.as_deref(),
        args.yes,
        true,
    )
    .await?;
    let name = name.context("approved without a machine")?;
    let machine = machine(metadata, &args.flake, &name)?;
    check_push(&name, machine)?;
    let target = serde_json::json!({
        "id": conn.remote_id().to_string(),
        "label": label,
        "inventory": inventory,
    });
    let mut hooks = start_hooks(args, &name, machine, target, programs).await?;
    hooks.after(Step::Approve).await?;
    if let Some(path) = &machine.facter.report_path {
        controller::fetch_facter_report(stream, &args.flake, path)
            .await
            .context("getting the facter report")?;
    }
    hooks.after(Step::Facter).await?;
    // Before building: the configuration (or a rekeying hook) may use it.
    let pulled = machine_host_key(stream, &args.flake, &name, machine, &mut hooks).await?;
    hooks.context.host_key = pulled.as_ref().map(ToString::to_string);
    hooks.after(Step::HostKey).await?;

    let configuration = Configuration::new(&args.flake, &machine.configuration)?;
    let facts = configuration.facts().await?;
    if facts.system != inventory.system {
        bail!(
            "nixosConfigurations.{} is for {}, but the target is {}",
            machine.configuration,
            facts.system,
            sanitise(&inventory.system, 32)
        );
    }
    preflight::check_boot(&facts.boot, &inventory.firmware, &machine.configuration)?;
    let host_key = host_key_path(pulled, &facts, &machine.configuration)?;
    let disko = facts
        .disko
        .with_context(|| no_disko(&machine.configuration))?;
    let needs = secrets::needs(&disko.encryption)?;
    secrets::check_available(&needs, !machine.hooks.is_empty())?;
    let choices = choose_disks(
        &disko,
        machine.disks.file.is_some(),
        &inventory.disks,
        &mut requested,
        args.yes,
    )
    .await?;
    let formatting = choices.iter().map(|c| c.disk.as_str()).collect();
    plan::check_name_clashes(&plan::created_names(&disko), &formatting, &inventory.disks)
        .map_err(|reason| anyhow!(reason))?;
    if let Some(file) = &machine.disks.file {
        record_pins(&configuration, &args.flake, file, &choices).await?;
    }
    hooks.context.disks = Some(serde_json::to_value(&choices)?);
    hooks.after(Step::Disks).await?;

    // Before the build, so it can run unattended.
    let key_files = secrets::gather(needs, hooks.take_disk_keys()).await?;

    eprintln!("\nBuilding nixosConfigurations.{}", machine.configuration);
    let built = configuration.build().await?;
    hooks.context.system = Some(built.toplevel.clone());
    hooks.after(Step::Build).await?;
    Ok(Plan {
        label,
        name,
        configuration: machine.configuration.clone(),
        disks: inventory.disks,
        choices,
        built,
        disko,
        ssh: facts.ssh,
        host_key,
        key_files,
        hooks,
    })
}

/// The hooks of the approved machine, ready to run. `programs`: resolved
/// before pairing, if the machine was known then.
async fn start_hooks(
    args: &InstallArgs,
    name: &str,
    machine: &Machine,
    target: serde_json::Value,
    programs: Option<BTreeMap<String, PathBuf>>,
) -> Result<hooks::Runner> {
    let programs = match programs {
        Some(programs) => programs,
        None => hooks::resolve(&args.flake, &machine.hooks).await?,
    };
    hooks::Runner::new(
        machine.hooks.clone(),
        programs,
        metadata::local_flake_dir(&args.flake).map_or_else(std::env::current_dir, Ok)?,
        hooks::Context {
            machine: name.to_string(),
            configuration: machine.configuration.clone(),
            flake: args.flake.clone(),
            target,
            ..hooks::Context::default()
        },
    )
}

/// Pull mode: the pulled key, and where the configuration wants it.
fn host_key_path(
    pulled: Option<PublicKey>,
    facts: &Facts,
    configuration: &str,
) -> Result<Option<(PublicKey, String)>> {
    match (pulled, &facts.ssh.host_key_path) {
        (None, _) => Ok(None),
        (Some(key), Some(path)) => Ok(Some((key, path.clone()))),
        (Some(_), None) => bail!(
            "hostKey.mode = \"pull\" needs an ed25519 key in nixosConfigurations.{configuration}'s \
             services.openssh.hostKeys"
        ),
    }
}

/// From the confirmation to erase on: format, send the system, install,
/// and leave the target ready to boot.
async fn apply(
    stream: &mut Stream,
    conn: &Connection,
    peer: &Peer,
    args: &InstallArgs,
    plan: Plan,
) -> Result<Installed> {
    let Plan {
        label,
        name,
        configuration,
        disks,
        choices,
        built,
        disko,
        ssh,
        host_key,
        key_files,
        mut hooks,
    } = plan;
    confirm_erase(&disks, &choices, &name, args.yes, args.wipe).await?;

    eprintln!("Sending the disko script");
    push::copy(
        conn,
        &StreamRequest::LiveStore,
        std::slice::from_ref(&built.disko_script),
    )
    .await
    .context("pushing the disko script into the target's live store")?;
    let labels = plan::created_names(&disko).into_iter().collect();
    prepare_disks(stream, &choices, labels).await?;
    eprintln!("Formatting");
    run_disko(stream, &built.disko_script, key_files).await?;
    let root = disko.root_mount_point;
    hooks.context.root = Some(root.clone());
    hooks.after(Step::Format).await?;
    place_files(stream, &root, &mut hooks).await?;

    let fstype = push::prepare_disk_store(stream, &root).await?;
    eprintln!("Sending the system into {root} ({fstype})");
    let seconds = push::copy(
        conn,
        &StreamRequest::Store { root: root.clone() },
        std::slice::from_ref(&built.toplevel),
    )
    .await?;
    hooks.after(Step::Copy).await?;
    place_files(stream, &root, &mut hooks).await?;
    eprintln!("Installing the boot loader");
    let (expected, key_path) = host_key.unzip();
    install_system(stream, &root, &built.toplevel, key_path).await?;
    hooks.after(Step::Install).await?;
    place_files(stream, &root, &mut hooks).await?;

    let mut report = report(
        conn,
        label,
        &name,
        configuration,
        &choices,
        &built.toplevel,
        &root,
        fstype,
        seconds,
        args.then,
    )?;
    let shutdown = match args.then {
        Then::Stay => {
            eprintln!("Installed; {root} stays mounted on the target.");
            return Ok(Installed {
                report,
                check: None,
                hooks,
            });
        }
        Then::Reboot => Shutdown::Reboot,
        Then::Poweroff => Shutdown::PowerOff,
    };
    let live = finish(stream, &root, &disko.zpools, shutdown).await?;
    hooks.context.root = None;
    hooks.after(Step::Finish).await?;
    no_files_left(&mut hooks, Step::Finish)?;
    let check = if ssh.enable {
        Some(Check {
            addrs: peer.addrs.iter().copied().collect(),
            port: ssh.ports.first().copied().unwrap_or(22),
            expected,
            live,
            machine: name,
            encrypted: !(disko.encryption.luks.is_empty() && disko.encryption.zfs.is_empty()),
        })
    } else {
        report.insert(
            "hostKey".into(),
            serde_json::json!({ "skipped": "services.openssh is not enabled" }),
        );
        None
    };
    Ok(Installed {
        report,
        check,
        hooks,
    })
}

/// The result so far, as printed at the end.
#[allow(clippy::too_many_arguments, reason = "the result's fields, named")]
fn report(
    conn: &Connection,
    label: String,
    name: &str,
    configuration: String,
    choices: &[Choice],
    system: &str,
    root: &str,
    fstype: String,
    seconds: f64,
    then: Then,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut report = serde_json::Map::new();
    for (key, value) in [
        ("target", conn.remote_id().to_string().into()),
        ("label", label.into()),
        ("machine", name.into()),
        ("configuration", configuration.into()),
        ("disks", serde_json::to_value(choices)?),
        ("system", system.into()),
        ("root", root.into()),
        ("fstype", fstype.into()),
        ("pushSeconds", seconds.into()),
        ("then", format!("{then:?}").to_ascii_lowercase().into()),
    ] {
        report.insert(key.into(), value);
    }
    Ok(report)
}

/// Puts the files hooks wrote so far into the installed system.
async fn place_files(stream: &mut Stream, root: &str, hooks: &mut hooks::Runner) -> Result<()> {
    let files = hooks.take_files();
    if files.is_empty() {
        return Ok(());
    }
    eprintln!(
        "Putting {} file(s) from hooks into the installed system",
        files.len()
    );
    write_frame(
        stream,
        &ControllerMsg::PlaceFiles {
            root: root.into(),
            files,
        },
    )
    .await?;
    let reply = tokio::time::timeout(PREPARE_TIMEOUT, read_frame(stream, MAX_TARGET_FRAME))
        .await
        .context("timed out waiting for the target to place the files")??;
    match reply {
        TargetMsg::FilesPlaced => Ok(()),
        TargetMsg::FilesRefused { reason } => {
            bail!(
                "the target refused the hooks' files: {}",
                sanitise(&reason, 300)
            )
        }
        _ => bail!("unexpected reply to placing files"),
    }
}

/// Has the target generate the host key and records its public half.
/// The machine's host key, by its `hostKey.mode`: none, generated on the
/// target (pull), or from a hook (push).
async fn machine_host_key(
    stream: &mut Stream,
    flake: &str,
    name: &str,
    machine: &Machine,
    hooks: &mut hooks::Runner,
) -> Result<Option<PublicKey>> {
    let pushed = hooks.take_host_key();
    Ok(match (machine.host_key.mode, pushed) {
        (HostKeyMode::None, None) => None,
        (HostKeyMode::Pull, None) => Some(host_key(stream, flake, name, machine, None).await?),
        (HostKeyMode::Push, Some(private)) => {
            Some(host_key(stream, flake, name, machine, Some(private)).await?)
        }
        (HostKeyMode::Push, None) => bail!(
            "hostKey.mode = \"push\", but no hook wrote $PIXIE_OUT/host-key before `host-key`"
        ),
        (mode, Some(_)) => bail!(
            "a hook wrote $PIXIE_OUT/host-key, but hostKey.mode is \"{}\", not \"push\"",
            format!("{mode:?}").to_ascii_lowercase()
        ),
    })
}

/// The host key: generated on the target (pull mode), or `pushed` there from
/// a hook. Records its public half in the repository if the machine says
/// where.
async fn host_key(
    stream: &mut Stream,
    flake: &str,
    name: &str,
    machine: &Machine,
    pushed: Option<Zeroizing<Vec<u8>>>,
) -> Result<PublicKey> {
    let how = if pushed.is_some() { "pushed" } else { "pulled" };
    let request = match pushed {
        Some(private) => ControllerMsg::ProvideHostKey {
            private: Secret(private),
        },
        None => ControllerMsg::GenerateHostKey,
    };
    write_frame(stream, &request).await?;
    let reply = tokio::time::timeout(PREPARE_TIMEOUT, read_frame(stream, MAX_TARGET_FRAME))
        .await
        .context("timed out waiting for the target's host key")??;
    let key = match reply {
        TargetMsg::HostKey { public } => {
            PublicKey::parse(&public).context("the target's host key")?
        }
        TargetMsg::HostKeyFailed { error } => bail!(
            "the target has no host key ({how}): {}",
            sanitise(&error, 300)
        ),
        _ => bail!("unexpected reply to the host key request"),
    };
    info!(%key, how, "host key");
    if let Some(path) = &machine.host_key.public_key_path {
        let root = repo_root(flake)?;
        let file = metadata::repo_path(&root, path)?;
        let line = key.to_line(Some(&format!("root@{name}")));
        metadata::write_into_repo(&root, &file, line.as_bytes()).await?;
        info!(%path, "recorded the host key");
    }
    Ok(key)
}

/// Reads `Output` frames until the reply that ends them.
async fn follow(stream: &mut Stream) -> Result<(TargetMsg, Tail)> {
    let mut tail = Tail::default();
    loop {
        match read_frame(stream, MAX_TARGET_FRAME).await? {
            TargetMsg::Output { chunk } => {
                trace!(bytes = chunk.len(), "output");
                tail.push(&chunk);
            }
            reply => return Ok((reply, tail)),
        }
    }
}

async fn install_system(
    stream: &mut Stream,
    root: &str,
    system: &str,
    host_key_path: Option<String>,
) -> Result<()> {
    write_frame(
        stream,
        &ControllerMsg::Install {
            root: root.into(),
            system: system.into(),
            host_key_path,
        },
    )
    .await?;
    match follow(stream).await? {
        (TargetMsg::InstallDone, _) => Ok(()),
        (TargetMsg::InstallFailed { error }, tail) => {
            eprintln!("{}", tail.lines(OUTPUT_TAIL_LINES));
            bail!("installing failed: {}", sanitise(&error, 300))
        }
        _ => bail!("unexpected message while installing"),
    }
}

/// Unmounts and exports on the target. Returns the installer's own host key.
async fn finish(
    stream: &mut Stream,
    root: &str,
    zpools: &[String],
    then: Shutdown,
) -> Result<Option<PublicKey>> {
    write_frame(
        stream,
        &ControllerMsg::Finish {
            root: root.into(),
            zpools: zpools.to_vec(),
            then,
        },
    )
    .await?;
    let reply = tokio::time::timeout(FINISH_TIMEOUT, read_frame(stream, MAX_TARGET_FRAME))
        .await
        .context("timed out waiting for the target to unmount")??;
    match reply {
        TargetMsg::Finished { live_host_key } => {
            Ok(live_host_key.and_then(|key| PublicKey::parse(&key).ok()))
        }
        TargetMsg::FinishFailed { error } => {
            bail!("the target could not unmount: {}", sanitise(&error, 300))
        }
        _ => bail!("unexpected reply to finishing"),
    }
}

/// `ROLE=DISK` arguments, each role at most once.
pub fn parse_disk_args(args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut requested = BTreeMap::new();
    for arg in args {
        let (role, disk) = arg
            .split_once('=')
            .filter(|(role, disk)| !role.is_empty() && !disk.is_empty())
            .with_context(|| format!("--disk {arg}: expected ROLE=DISK, e.g. main=nvme0n1"))?;
        if requested
            .insert(role.to_string(), disk.to_string())
            .is_some()
        {
            bail!("--disk {role}=... is given twice");
        }
    }
    Ok(requested)
}

fn machine<'a>(metadata: &'a Metadata, flake: &str, name: &str) -> Result<&'a Machine> {
    metadata.machines.get(name).with_context(|| {
        let known: Vec<&str> = metadata.machines.keys().map(String::as_str).collect();
        format!(
            "{flake} has no machine `{name}`; it has: {}",
            known.join(", ")
        )
    })
}

fn no_disko(configuration: &str) -> String {
    format!(
        "nixosConfigurations.{configuration} doesn't use disko; nix-pixie formats the target with \
         the configuration's disko.devices"
    )
}

async fn disko_of(configuration: &Configuration) -> Result<Disko> {
    let facts = configuration.facts().await?;
    facts.disko.with_context(|| no_disko(configuration.name()))
}

fn repo_root(flake: &str) -> Result<PathBuf> {
    metadata::local_flake_dir(flake).with_context(|| {
        format!("{flake} is not a local checkout, so the chosen disks can't be recorded in it")
    })
}

/// Resolves the disk plan, asking the operator where it has to.
async fn choose_disks(
    disko: &Disko,
    pinned: bool,
    present: &[Disk],
    requested: &mut BTreeMap<String, String>,
    yes: bool,
) -> Result<Vec<Choice>> {
    let slots = plan::slots(disko, pinned, present)?;
    loop {
        let (role, candidates) = match plan::resolve(&slots, present, requested)? {
            Resolution::Done(choices) => return Ok(choices),
            Resolution::Ask { role, candidates } => (role, candidates),
        };
        let described: Vec<String> = candidates
            .iter()
            .filter_map(|name| present.iter().find(|d| &d.name == name))
            .map(describe_disk)
            .collect();
        if yes || !std::io::stdin().is_terminal() {
            bail!(
                "choose the disk for {role} with --disk {role}=<disk>. Candidates:\n  {}",
                described.join("\n  ")
            );
        }
        eprintln!("\nWhich disk is {role}?");
        for (i, line) in described.iter().enumerate() {
            eprintln!("  {}) {line}", i + 1);
        }
        let answer = read_line("Number: ").await?;
        let pick = answer
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| candidates.get(i))
            .with_context(|| format!("{:?} is not one of the numbers", answer.trim()))?;
        requested.insert(role, pick.clone());
    }
}

/// Pinned mode: writes the chosen disks into the repository, and checks that
/// the configuration now uses them.
async fn record_pins(
    configuration: &Configuration,
    flake: &str,
    file: &str,
    choices: &[Choice],
) -> Result<()> {
    let root = repo_root(flake)?;
    let path = metadata::repo_path(&root, file)?;
    metadata::write_into_repo(&root, &path, &plan::pins_json(choices)?).await?;
    info!(file, "recorded the chosen disks");
    let disko = disko_of(configuration).await?;
    plan::verify_pins(&disko, choices, file)
}

/// Shows what gets erased and asks, unless the flags already answered.
async fn confirm_erase(
    disks: &[Disk],
    choices: &[Choice],
    machine: &str,
    yes: bool,
    wipe: bool,
) -> Result<()> {
    let chosen: Vec<(&Choice, &Disk)> = choices
        .iter()
        .filter_map(|c| Some((c, disks.iter().find(|d| d.name == c.disk)?)))
        .collect();
    let with_data = chosen.iter().any(|(_, d)| d.has_data());
    eprintln!("\nThis erases everything on:");
    for (choice, disk) in &chosen {
        eprintln!("  {:<10} {}", choice.role, describe_disk(disk));
    }
    if with_data {
        if wipe {
            return Ok(());
        }
        if yes || !std::io::stdin().is_terminal() {
            bail!("there is data on these disks; pass --wipe to erase it");
        }
        let answer = read_line(&format!(
            "Type the machine's name ({machine}) to erase it: "
        ))
        .await?;
        if answer.trim() != machine {
            bail!("not erasing");
        }
    } else if !(yes || confirm("Format these disks? [y/N] ").await?) {
        bail!("not formatting");
    }
    Ok(())
}

async fn read_line(prompt: &str) -> Result<String> {
    use tokio::io::AsyncBufReadExt;
    eprint!("{prompt}");
    let mut line = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await
        .context("reading the answer")?;
    Ok(line)
}

async fn prepare_disks(stream: &mut Stream, choices: &[Choice], labels: Vec<String>) -> Result<()> {
    let disks = choices
        .iter()
        .map(|c| DiskAssignment {
            role: c.role.clone(),
            device: c.disk.clone(),
            link: c.link,
        })
        .collect();
    write_frame(stream, &ControllerMsg::PrepareDisks { disks, labels }).await?;
    let reply = tokio::time::timeout(PREPARE_TIMEOUT, read_frame(stream, MAX_TARGET_FRAME))
        .await
        .context("timed out waiting for the target to check its disks")??;
    match reply {
        TargetMsg::DisksReady => Ok(()),
        TargetMsg::DisksRefused { reason } => {
            bail!("the target refused to format: {}", sanitise(&reason, 300))
        }
        _ => bail!("unexpected reply to the disk check"),
    }
}

/// Runs disko on the target. Its output is kept, and shown if it fails.
async fn run_disko(stream: &mut Stream, script: &str, key_files: Vec<KeyFile>) -> Result<()> {
    write_frame(
        stream,
        &ControllerMsg::RunDisko {
            script: script.into(),
            key_files,
        },
    )
    .await?;
    match follow(stream).await? {
        (TargetMsg::DiskoDone, _) => Ok(()),
        (TargetMsg::DiskoFailed { error }, tail) => {
            eprintln!("{}", tail.lines(OUTPUT_TAIL_LINES));
            bail!("formatting failed: {}", sanitise(&error, 300))
        }
        _ => bail!("unexpected message while formatting"),
    }
}

/// The end of a byte stream, for showing the last lines of it.
#[derive(Default)]
pub struct Tail {
    bytes: Vec<u8>,
}

impl Tail {
    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if let Some(excess) = self.bytes.len().checked_sub(OUTPUT_TAIL_BYTES) {
            self.bytes.drain(..excess);
        }
    }

    /// The last `n` lines, with anything but printable text removed: the
    /// output comes from the target.
    pub fn lines(&self, n: usize) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let lines: Vec<String> = text.lines().map(|line| sanitise(line, 300)).collect();
        lines
            .get(lines.len().saturating_sub(n)..)
            .unwrap_or_default()
            .join("\n")
    }
}
