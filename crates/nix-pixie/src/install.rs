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
    ControllerMsg, DiskAssignment, MAX_TARGET_FRAME, StreamRequest, TargetMsg,
};
use nix_pixie_proto::frame::{read_frame, write_frame};
use nix_pixie_proto::inventory::Disk;
use tracing::{info, trace};

use crate::controller::{self, Approved, confirm, describe_disk};
use crate::discovery::{Discovery, Method, Peer};
use crate::identity;
use crate::metadata::{self, Machine, Metadata};
use crate::net::{self, Stream, sanitise};
use crate::plan::{self, Choice, Resolution};
use crate::push;
use crate::system::{Configuration, Disko};

/// For the agent's checks before formatting (a fresh lsblk).
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of disko's output to keep for showing a failure.
const DISKO_TAIL_BYTES: usize = 64 * 1024;
const DISKO_TAIL_LINES: usize = 40;

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
    if let Some(name) = &args.machine {
        let machine = machine(&metadata, &args.flake, name)?;
        let disko = disko_of(&Configuration::new(&args.flake, &machine.configuration)?).await?;
        plan::check_requested(&disko, machine.disks.file.is_some(), &requested)?;
        if machine.disks.file.is_some() {
            repo_root(&args.flake)?;
        }
    }

    let endpoint = net::bind(identity::load_or_create(&args.identity)?, 0).await?;
    let mut discovery = Discovery::start(args.method).await?;
    let result = tokio::select! {
        r = async {
            let (conn, mut stream, peer) = controller::pair_with_target(
                &endpoint, &mut discovery, &code, nameplate, args.timeout,
            )
            .await?;
            let result = session(&mut stream, &conn, &peer, &metadata, &args, requested).await;
            net::finish(&mut stream).await;
            conn.close(VarInt::from_u32(0), b"done");
            result
        } => r,
        () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
    };
    discovery.shutdown();
    endpoint.close().await;
    result
}

/// Everything after pairing.
async fn session(
    stream: &mut Stream,
    conn: &Connection,
    peer: &Peer,
    metadata: &Metadata,
    args: &InstallArgs,
    mut requested: BTreeMap<String, String>,
) -> Result<()> {
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
    if let Some(path) = &machine.facter.report_path {
        controller::fetch_facter_report(stream, &args.flake, path)
            .await
            .context("getting the facter report")?;
    }

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
    let disko = facts
        .disko
        .with_context(|| no_disko(&machine.configuration))?;
    let choices = choose_disks(
        &disko,
        machine.disks.file.is_some(),
        &inventory.disks,
        &mut requested,
        args.yes,
    )
    .await?;
    if let Some(file) = &machine.disks.file {
        record_pins(&configuration, &args.flake, file, &choices).await?;
    }

    eprintln!("\nBuilding nixosConfigurations.{}", machine.configuration);
    let built = configuration.build().await?;
    confirm_erase(&inventory.disks, &choices, &name, args.yes, args.wipe).await?;

    eprintln!("Sending the disko script");
    push::copy(
        conn,
        &StreamRequest::LiveStore,
        std::slice::from_ref(&built.disko_script),
    )
    .await
    .context("pushing the disko script into the target's live store")?;
    prepare_disks(stream, &choices).await?;
    eprintln!("Formatting");
    run_disko(stream, &built.disko_script).await?;

    let root = disko.root_mount_point;
    let fstype = push::prepare_disk_store(stream, &root).await?;
    eprintln!("Sending the system into {root} ({fstype})");
    let seconds = push::copy(
        conn,
        &StreamRequest::Store { root: root.clone() },
        std::slice::from_ref(&built.toplevel),
    )
    .await?;

    let report = serde_json::json!({
        "target": conn.remote_id().to_string(),
        "label": label,
        "machine": name,
        "configuration": machine.configuration,
        "disks": choices,
        "system": built.toplevel,
        "root": root,
        "fstype": fstype,
        "pushSeconds": seconds,
    });
    println!("{report}");
    Ok(())
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

async fn prepare_disks(stream: &mut Stream, choices: &[Choice]) -> Result<()> {
    let disks = choices
        .iter()
        .map(|c| DiskAssignment {
            role: c.role.clone(),
            device: c.disk.clone(),
            link: c.link,
        })
        .collect();
    write_frame(stream, &ControllerMsg::PrepareDisks { disks }).await?;
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
async fn run_disko(stream: &mut Stream, script: &str) -> Result<()> {
    write_frame(
        stream,
        &ControllerMsg::RunDisko {
            script: script.into(),
        },
    )
    .await?;
    let mut tail = Tail::default();
    loop {
        match read_frame(stream, MAX_TARGET_FRAME).await? {
            TargetMsg::DiskoOutput { chunk } => {
                trace!(bytes = chunk.len(), "disko output");
                tail.push(&chunk);
            }
            TargetMsg::DiskoDone => return Ok(()),
            TargetMsg::DiskoFailed { error } => {
                eprintln!("{}", tail.lines(DISKO_TAIL_LINES));
                bail!("formatting failed: {}", sanitise(&error, 300));
            }
            _ => bail!("unexpected message while formatting"),
        }
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
        if let Some(excess) = self.bytes.len().checked_sub(DISKO_TAIL_BYTES) {
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
