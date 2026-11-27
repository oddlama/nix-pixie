//! Controller commands: pairing, approval, and the scriptable building
//! blocks of an install. They talk to the operator through [`crate::ui`].
//!
//! The controller only ever dials out. It needs no open ports on the
//! operator's machine.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointId, TransportAddr};
use nix_pixie_proto::announce::Pairing;
use nix_pixie_proto::code::{Code, NameplateSpace};
use nix_pixie_proto::ctrl::{ControllerMsg, MAX_TARGET_FRAME, TargetMsg};
use nix_pixie_proto::frame::read_frame;
use nix_pixie_proto::inventory::{Disk, Inventory};
use nix_pixie_proto::pair::{PairError, Side};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::agent::MAX_FAILURES;
use crate::discovery::{self, Discovery, Method, Peer};
use crate::exit;
use crate::identity;
use crate::link::Link;
use crate::metadata::{self, Metadata, Suggestion};
use crate::net::{self, DialError, Relays, Route, Stream, sanitise};
use crate::push;
use crate::state::write_private;
use crate::ui::{self, Tone};

/// How long to collect a newly found target's addresses before dialing it.
const SETTLE: Duration = Duration::from_millis(600);
/// For the target's hello and inventory (lsblk and sysfs reads).
const INTRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long setting the target's clock may take (and `hwclock` with it).
const CLOCK_TIMEOUT: Duration = Duration::from_secs(30);

pub fn print_id(identity_path: &Path) -> Result<()> {
    println!("{}", identity::load_or_create(identity_path)?.public());
    Ok(())
}

/// How `nix-pixie controller pair` was invoked.
pub struct PairArgs {
    pub identity: PathBuf,
    /// `None`: asked for on the terminal, right before pairing, unless
    /// there is a controller's code.
    pub code: Option<String>,
    /// A controller's code to pair with instead.
    pub controller_code_file: Option<PathBuf>,
    /// A serial port to hand the target a controller's code through: ours
    /// from `controller_code_file`, or a new one.
    pub serial: Option<PathBuf>,
    pub timeout: Duration,
    pub method: Method,
    /// Flake with the `pixie` output, for matching and writing reports.
    pub flake: Option<String>,
    /// Install the target as this machine of the flake.
    pub machine: Option<String>,
    /// Approve without asking.
    pub yes: bool,
    /// Store paths to copy (with their closures) into the target's disk.
    pub push: Vec<String>,
    /// Where the target's filesystems are mounted.
    pub store_root: String,
    /// Probe these hosts instead of searching the local network.
    pub targets: Vec<String>,
    /// Where to meet targets on other networks.
    pub relays: Relays,
}

/// Pairs with the target showing a code (default mode), shows what it is,
/// asks the operator to approve it, and prints one JSON object describing it.
pub async fn pair(args: PairArgs) -> Result<()> {
    exit::reached(exit::Phase::Checking);
    // A typo in a given code fails at once.
    if let Some(code) = &args.code {
        target_code(code)?;
    }
    if let Some(path) = &args.controller_code_file {
        controller_code_file(path)?;
    }
    // Check everything we can first: a mistake shouldn't cost a pairing code.
    push::check_local_paths(&args.push).await?;
    let flake = match &args.flake {
        Some(flake) => Some((flake.as_str(), metadata::load(flake).await?)),
        None => None,
    };
    match (&args.machine, &flake) {
        (Some(_), None) => bail!("--machine needs --flake"),
        (Some(machine), Some((flake, metadata))) if !metadata.machines.contains_key(machine) => {
            let known: Vec<&str> = metadata.machines.keys().map(String::as_str).collect();
            bail!(
                "{flake} has no machine `{machine}`; it has: {}",
                known.join(", ")
            );
        }
        _ => {}
    }

    ui::steps(pair_steps(false, !args.push.is_empty()));
    let code = pair_code(
        args.code.as_deref(),
        args.controller_code_file.as_deref(),
        args.serial.is_some(),
        args.method,
        &args.targets,
    )
    .await?;
    let search = Search {
        identity: &args.identity,
        method: args.method,
        targets: &args.targets,
        relays: &args.relays,
    };
    exit::reached(exit::Phase::Pairing);
    let step = ui::start_with("pair", search.looking(code));
    let (endpoint, mut discovery) = search.start(code).await?;
    let result = tokio::select! {
        r = async {
            let (conn, stream, peer) = pair_with_target(
                &endpoint,
                &mut discovery,
                code,
                args.timeout,
                args.serial.as_deref(),
            )
            .await?;
            exit::reached(exit::Phase::Paired);
            step.done(describe_peer(&conn, &peer));
            let link = Link::new(&endpoint, &peer, conn, stream);
            let result = review(&link, &peer, flake.as_ref(), &args).await;
            link.close().await;
            result
        } => r,
        () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
    };
    discovery.shutdown();
    endpoint.close().await;
    result
}

/// Targets nearby that show a code, as discovery finds them while the
/// operator types one: for completing nameplates.
pub struct Nearby {
    pub found: Arc<Mutex<ui::code::Nearby>>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Nearby {
    /// Looks around with `method`, or asks `targets` only.
    pub fn start(method: Method, targets: &[String]) -> Self {
        let found = Arc::new(Mutex::new(ui::code::Nearby::new()));
        let (stop, mut stopped) = oneshot::channel();
        let (into, targets) = (found.clone(), targets.to_vec());
        let task = tokio::spawn(async move {
            let Ok(targets) = discovery::resolve_targets(&targets).await else {
                return;
            };
            let Ok(mut discovery) = Discovery::start(method, targets).await else {
                return;
            };
            loop {
                tokio::select! {
                    peer = discovery.next() => match peer {
                        Ok(peer) => {
                            if let Pairing::ShowsCode { nameplate } = peer.pairing
                                && let Ok(mut found) = into.lock()
                            {
                                found.insert(nameplate, peer.label);
                            }
                        }
                        Err(_) => break,
                    },
                    _ = &mut stopped => break,
                }
            }
            discovery.shutdown();
        });
        Self { found, stop, task }
    }

    /// Stops looking.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

/// The code shown on the target: as given, or asked for on the terminal
/// with nameplates completing from the targets nearby.
pub async fn code_or_ask(given: Option<Code>, method: Method, targets: &[String]) -> Result<Code> {
    if let Some(code) = given {
        return Ok(code);
    }
    let nearby = Nearby::start(method, targets);
    let asked = ui::code::ask_target_code(nearby.found.clone()).await;
    nearby.stop().await;
    asked
}

/// A target's code as given on the command line.
///
/// # Errors
///
/// If it doesn't parse, or has no nameplate (a controller's code).
pub fn target_code(given: &str) -> Result<Code> {
    let code = Code::parse(given)?;
    code.nameplate().context(ui::code::NAMEPLATE_NEEDED)?;
    Ok(code)
}

/// The steps of `controller pair`: with the facter report once the
/// approved machine turns out to want one, and with the push if asked for.
fn pair_steps(facter: bool, push: bool) -> Vec<(String, String)> {
    let mut steps = vec![
        ("pair", "Pair with the target"),
        ("approve", "Approve the target"),
    ];
    if facter {
        steps.push(("facter", "Get the facter report"));
    }
    if push {
        steps.push(("push", "Send the store paths"));
    }
    steps
        .into_iter()
        .map(|(key, label)| (key.to_string(), label.to_string()))
        .collect()
}

/// Where and how the controller looks for the target showing a code.
pub struct Search<'a> {
    pub identity: &'a Path,
    pub method: Method,
    /// Probe these hosts instead of searching the local network.
    pub targets: &'a [String],
    pub relays: &'a Relays,
}

/// The code a pairing proves, and whose it is.
#[derive(Debug, Clone, Copy)]
pub enum PairCode {
    /// Shown by the target, starting with its nameplate; typed here.
    Target { code: Code, nameplate: u32 },
    /// Ours (`--controller-code-file`, or new for `--serial`), handed to the
    /// target by other means: the kernel command line of an installer booted
    /// over USB, its serial port. The target waits for this controller to
    /// dial it.
    Controller(Code),
}

impl PairCode {
    /// The code's nameplate, if it has one.
    fn nameplate(self) -> Option<u32> {
        match self {
            Self::Target { nameplate, .. } => Some(nameplate),
            Self::Controller(_) => None,
        }
    }

    /// A rendezvous to look for the target at, with relays.
    fn at_relay(self) -> Option<u32> {
        self.nameplate()
            .filter(|n| NameplateSpace::of(*n) == Some(NameplateSpace::Relay))
    }

    /// Which target, for messages.
    fn which(self) -> String {
        match self {
            Self::Target { nameplate, .. } => format!("target with nameplate {nameplate}"),
            Self::Controller(_) => "target waiting for this controller's code".into(),
        }
    }
}

/// The code of a target or of this controller, as given or asked for. With
/// `serial` (the code goes to the target over a serial port), a
/// controller's: from `controller`, or else a new one.
///
/// # Errors
///
/// If a file can't be read, or a code doesn't parse or is of the wrong kind.
pub async fn pair_code(
    target: Option<&str>,
    controller: Option<&Path>,
    serial: bool,
    method: Method,
    targets: &[String],
) -> Result<PairCode> {
    if let Some(path) = controller {
        return Ok(PairCode::Controller(controller_code_file(path)?));
    }
    if serial {
        return Ok(PairCode::Controller(Code::generate(None)?));
    }
    let code = code_or_ask(target.map(target_code).transpose()?, method, targets).await?;
    let nameplate = code.nameplate().context(ui::code::NAMEPLATE_NEEDED)?;
    Ok(PairCode::Target { code, nameplate })
}

/// A controller's code from `path`: three words, without a nameplate.
///
/// # Errors
///
/// If the file can't be read, or doesn't hold a controller's code.
pub fn controller_code_file(path: &Path) -> Result<Code> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the code from {}", path.display()))?;
    let code =
        Code::parse(text.trim()).with_context(|| format!("the code in {}", path.display()))?;
    if code.nameplate().is_some() {
        bail!(
            "{} holds a target's code (it starts with a number); a controller's code is three \
             words, e.g. from `nix-pixie controller new-code`",
            path.display()
        );
    }
    Ok(code)
}

impl Search<'_> {
    /// Where it looks for the target, for the operator.
    pub fn looking(&self, code: PairCode) -> String {
        if !self.targets.is_empty() {
            return format!("asking {}", self.targets.join(", "));
        }
        match code {
            PairCode::Target { nameplate, .. }
                if self.relays.is_enabled() && code.at_relay().is_some() =>
            {
                format!("looking for {nameplate} on this network and at the relay")
            }
            PairCode::Target { nameplate, .. } => {
                format!("looking for {nameplate} on this network")
            }
            PairCode::Controller(_) => "looking for a target waiting for this code".into(),
        }
    }

    /// Binds the controller's endpoint and starts looking for the target:
    /// at the given hosts, or else on the local network and, if the code's
    /// nameplate is one for relays, at its rendezvous.
    pub async fn start(&self, code: PairCode) -> Result<(Endpoint, Discovery)> {
        let targets = discovery::resolve_targets(self.targets).await?;
        // A target with a short nameplate (or none) isn't at any rendezvous,
        // so don't connect to relays for it.
        let rendezvous = code.at_relay().filter(|_| targets.is_empty());
        let relays = if rendezvous.is_some() {
            self.relays.clone()
        } else {
            Relays::Disabled
        };
        let secret = identity::load_or_create(self.identity)?;
        let endpoint = net::bind(secret, 0, &relays).await?;
        let mut discovery = Discovery::start(self.method, targets).await?;
        if let Some(nameplate) = rendezvous
            && relays.is_enabled()
        {
            discovery.search_relays(&endpoint, nameplate, &relays);
        }
        Ok((endpoint, discovery))
    }
}

/// Pairs with the first target that takes `pair`'s code within `timeout`.
/// A controller's code goes to the target over `serial` meanwhile, if
/// given.
pub async fn pair_with_target(
    endpoint: &Endpoint,
    discovery: &mut Discovery,
    pair: PairCode,
    timeout: Duration,
    serial: Option<&Path>,
) -> Result<(Connection, Stream, Peer)> {
    let _handing_over = match (serial, pair) {
        (Some(port), PairCode::Controller(code)) => {
            Some(AbortOnDrop(crate::serial::spawn(port.to_path_buf(), code)))
        }
        _ => None,
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut dialer = Dialer::default();
    let which = pair.which();
    let (wanted, side, code) = match pair {
        PairCode::Target { code, nameplate } => {
            (Pairing::ShowsCode { nameplate }, Side::TypedCode, code)
        }
        PairCode::Controller(code) => (Pairing::TypedControllerCode, Side::OwnsCode, code),
    };
    loop {
        let Ok(peer) = tokio::time::timeout_at(deadline, discovery.next()).await else {
            if dialer.paired_with_any() {
                bail!("no {which} accepted the code ({})", discovery.describe());
            }
            if let Some(error) = &dialer.last_connect_error {
                bail!("found a {which}, but could not connect to it: {error}");
            }
            let hint = pair
                .nameplate()
                .map_or("", |nameplate| discovery.hint(nameplate));
            bail!("no {which} found ({}). {hint}", discovery.describe());
        };
        let peer = peer?;
        // Nameplates are random and small, so two targets can share one; try
        // each. Every target waiting for a controller's code is tried too.
        if peer.pairing != wanted || !dialer.would_dial(&peer) {
            continue;
        }
        let peer = discovery.settle(peer.id, SETTLE).await.unwrap_or(peer);
        match dialer.attempt(endpoint, &peer, side, &code).await {
            Attempt::Paired(conn, stream) => return Ok((conn, stream, peer)),
            Attempt::TooManyFailures => bail!(
                "{MAX_FAILURES} machines rejected the code (a {which}). Someone may be \
                 impersonating targets; use a new code"
            ),
            Attempt::Skipped | Attempt::Failed => {}
        }
    }
}

/// A task that ends with its owner.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Reads the target's hello and inventory, the first two messages after
/// pairing.
pub(crate) async fn introduction(stream: &mut Stream) -> Result<(String, Inventory)> {
    let read = async {
        let TargetMsg::Hello { label } = read_frame(stream, MAX_TARGET_FRAME).await? else {
            bail!("the target did not start with a hello");
        };
        let TargetMsg::Inventory(inventory) = read_frame(stream, MAX_TARGET_FRAME).await? else {
            bail!("the target did not send its inventory");
        };
        Ok((sanitise(&label, 63), *inventory))
    };
    tokio::time::timeout(INTRODUCTION_TIMEOUT, read)
        .await
        .context("timed out waiting for the target to introduce itself")?
}

/// A target the operator approved.
pub struct Approved {
    /// Sanitised.
    pub label: String,
    pub inventory: Inventory,
    pub suggestions: Vec<Suggestion>,
    /// The flake's machine it was approved as.
    pub machine: Option<String>,
}

/// Reads the target's introduction, shows it, and asks the operator to
/// approve it (unless `yes`), telling the target the answer. The machine is
/// `chosen`, or the only one whose hints match. With `need_machine`, a
/// target without one is rejected.
pub async fn approve(
    link: &Link,
    peer: &Peer,
    metadata: Option<&Metadata>,
    chosen: Option<&str>,
    yes: bool,
    need_machine: bool,
) -> Result<Approved> {
    let (label, inventory) = link.introduction().await?;
    let suggestions = metadata.map_or_else(Vec::new, |m| m.suggest(&inventory));
    let machine = chosen
        .map(str::to_string)
        .or_else(|| match suggestions.as_slice() {
            [only] => Some(only.machine.clone()),
            _ => None,
        });
    ui::show(summary(
        &link.connection(),
        &label,
        peer,
        &inventory,
        &suggestions,
        machine.as_deref(),
    ));
    if need_machine && machine.is_none() {
        ui::commit();
        reject(link, "no machine chosen on the controller").await?;
        let known: Vec<&str> = metadata.map_or_else(Vec::new, |m| {
            m.machines.keys().map(String::as_str).collect()
        });
        bail!(
            "no machine of the flake matches this target; choose one with --machine ({})",
            known.join(", ")
        );
    }
    if yes {
        ui::commit();
    } else if !ui::confirm("Approve this machine? [y/N] ").await {
        reject(link, "rejected by the operator").await?;
        bail!("rejected");
    }
    link.tell(&ControllerMsg::Approve {
        machine: machine.clone(),
    })
    .await?;
    info!(target = %link.target(), ?machine, "approved");
    set_clock(link).await?;
    Ok(Approved {
        label,
        inventory,
        suggestions,
        machine,
    })
}

/// Sets the target's clock from ours, if it is off: before anything there
/// goes by the date (TLS, certificates, signed UEFI variables).
async fn set_clock(link: &Link) -> Result<()> {
    let request = ControllerMsg::SetClock {
        unix_ms: crate::clock::now_ms(),
    };
    match link
        .request(&request, CLOCK_TIMEOUT, "the target's clock")
        .await?
    {
        TargetMsg::Clock { off_ms, set: true } => {
            let off = crate::clock::describe(off_ms);
            info!(target = %link.target(), %off, "set the target's clock, which was off");
        }
        TargetMsg::Clock { off_ms, set: false } => {
            debug!(target = %link.target(), off_ms, "the target's clock is right");
        }
        TargetMsg::ClockFailed { error } => {
            warn!(target = %link.target(), "the target's clock may be off: {error}");
        }
        _ => bail!("unexpected reply to setting the clock"),
    }
    Ok(())
}

/// Tells the target it was rejected; it goes back to pairing.
async fn reject(link: &Link, reason: &str) -> Result<()> {
    link.tell(&ControllerMsg::Reject {
        reason: reason.into(),
    })
    .await
}

/// After pairing: approval, the facter report if the chosen machine wants
/// one, the optional push, and the result as JSON.
async fn review(
    link: &Link,
    peer: &Peer,
    flake: Option<&(&str, Metadata)>,
    args: &PairArgs,
) -> Result<()> {
    let step = ui::start("approve");
    let approved = approve(
        link,
        peer,
        flake.map(|(_, m)| m),
        args.machine.as_deref(),
        args.yes,
        false,
    )
    .await?;
    let Approved {
        label,
        inventory,
        suggestions,
        machine,
    } = approved;
    step.done(machine.clone().unwrap_or_default());

    let report_path = flake
        .zip(machine.as_ref())
        .and_then(|((flake, metadata), machine)| {
            let path = metadata.machines.get(machine)?.facter.report_path.clone()?;
            Some((*flake, path))
        });
    ui::steps(pair_steps(report_path.is_some(), !args.push.is_empty()));
    let facter = match report_path {
        Some((flake, path)) => {
            let step = ui::start("facter");
            let result = fetch_facter_report(link, flake, &path).await;
            match &result {
                Ok(path) => step.done(path.clone()),
                Err(e) => step.skip(format!("{e:#}")),
            }
            Some(result)
        }
        None => None,
    };
    let pushed = if args.push.is_empty() {
        None
    } else {
        let step = ui::start_with("push", format!("into {}", args.store_root));
        let pushed = push::push(link, &args.store_root, &args.push).await?;
        step.done(pushed.copied.what.clone());
        Some(pushed)
    };

    let report = serde_json::json!({
        "target": link.target().to_string(),
        "label": label,
        "addrs": peer.addrs.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "route": net::route(&link.connection()).kind(),
        "inventory": inventory,
        "suggestions": suggestions.iter().map(|s| serde_json::json!({
            "machine": s.machine, "reasons": s.reasons,
        })).collect::<Vec<_>>(),
        "machine": machine,
        "configuration": flake
            .zip(machine.as_ref())
            .and_then(|((_, metadata), machine)| metadata.machines.get(machine))
            .map(|m| m.configuration.clone()),
        "push": pushed.map(|p| serde_json::json!({
            "root": args.store_root, "fstype": p.fstype, "paths": args.push, "seconds": p.copied.seconds,
        })),
        "facterReport": facter.map(|f| match f {
            Ok(path) => serde_json::json!({ "path": path }),
            Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        }),
    });
    ui::finish();
    ui::print_result(&report.to_string());
    Ok(())
}

/// Asks the target for a facter report and writes it into the flake's
/// repository. Returns the path, relative to the flake.
pub async fn fetch_facter_report(link: &Link, flake: &str, report_path: &str) -> Result<String> {
    crate::facter::fetch(link, flake, report_path, crate::facter::Mode::Write, &[]).await?;
    Ok(report_path.to_string())
}

/// The paired target, for the operator: its label and how we reach it.
pub fn describe_peer(conn: &Connection, peer: &Peer) -> String {
    match net::route(conn) {
        Route::Relayed(relay) => format!("{}, relayed through {relay}", peer.label),
        _ => format!("{} at {}", peer.label, describe_route(conn, peer)),
    }
}

/// How we talk to the paired target, for the operator: the address in use,
/// or that a relay carries it.
fn describe_route(conn: &Connection, peer: &Peer) -> String {
    match net::route(conn) {
        Route::Unknown => net::dial_addrs(&peer.addrs)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        route => route.to_string(),
    }
}

/// What the operator sees before approving, one line each. Everything from
/// the target is sanitised again here: the controller doesn't trust the
/// target's cleanup.
fn summary(
    conn: &Connection,
    label: &str,
    peer: &Peer,
    inventory: &Inventory,
    suggestions: &[Suggestion],
    machine: Option<&str>,
) -> Vec<String> {
    let clean = |s: &str| sanitise(s, 128);
    let opt = |s: &Option<String>| s.as_deref().map(clean);
    let mut lines = vec![
        format!(
            "Paired with {} ({})",
            clean(label),
            describe_route(conn, peer)
        ),
        format!("  id        {}", conn.remote_id()),
    ];
    let firmware = match (inventory.firmware.uefi, inventory.firmware.secure_boot) {
        (false, _) => "BIOS",
        (true, Some(true)) => "UEFI, Secure Boot on",
        (true, Some(false)) => "UEFI, Secure Boot off",
        (true, None) => "UEFI",
    };
    lines.push(format!(
        "  system    {}, {} threads, {} RAM, {firmware}",
        clean(&inventory.system),
        inventory.cpu.threads,
        human_size(inventory.memory_bytes)
    ));
    let hardware: Vec<String> = [&inventory.dmi.vendor, &inventory.dmi.product]
        .into_iter()
        .filter_map(opt)
        .collect();
    if !hardware.is_empty() || inventory.dmi.serial.is_some() {
        let serial =
            opt(&inventory.dmi.serial).map_or_else(String::new, |s| format!(" (serial {s})"));
        lines.push(format!("  hardware  {}{serial}", hardware.join(" ")));
    }
    let tree = &inventory.device_tree;
    if tree.model.is_some() || tree.serial.is_some() {
        let model = opt(&tree.model).unwrap_or_else(|| "device tree".into());
        let serial = opt(&tree.serial).map_or_else(String::new, |s| format!(" (serial {s})"));
        lines.push(format!("  board     {model}{serial}"));
    }
    for nic in &inventory.nics {
        let link = if nic.link_up { "up" } else { "down" };
        lines.push(format!(
            "  nic       {} {} ({link})",
            clean(&nic.name),
            clean(&nic.mac)
        ));
    }
    for disk in &inventory.disks {
        lines.push(format!("  disk      {}", show_disk(disk)));
    }
    let doubt = |text: &str| ui::paint(text, Tone::Warn);
    let line = match (machine, suggestions) {
        (Some(machine), []) => format!("{machine} (chosen)"),
        (Some(machine), _) => {
            let reasons: Vec<&str> = suggestions
                .iter()
                .filter(|s| s.machine == machine)
                .flat_map(|s| s.reasons.iter().map(String::as_str))
                .collect();
            if reasons.is_empty() {
                format!(
                    "{machine} (chosen; {})",
                    doubt("the hints suggest otherwise")
                )
            } else {
                format!("{machine} (matched: {})", reasons.join(", "))
            }
        }
        (None, []) => doubt("none matched"),
        (None, many) => {
            let names: Vec<&str> = many.iter().map(|s| s.machine.as_str()).collect();
            doubt(&format!(
                "ambiguous: {}; pick one with --machine",
                names.join(", ")
            ))
        }
    };
    lines.push(format!("  machine   {line}"));
    lines
}

/// One line about `disk`: name, size, what it is, and what is on it.
/// Sanitised: everything in it comes from the target.
pub fn describe_disk(disk: &Disk) -> String {
    let (what, state, _) = disk_parts(disk);
    format!("{what} [{state}]")
}

/// [`describe_disk`] for the terminal: data on the disk stands out, a disk
/// in use steps back.
pub fn show_disk(disk: &Disk) -> String {
    let (what, state, tone) = disk_parts(disk);
    let state = format!("[{state}]");
    match tone {
        Some(tone) => format!("{what} {}", ui::paint(&state, tone)),
        None => format!("{what} {state}"),
    }
}

/// What a disk is, what is on it, and how that should look.
fn disk_parts(disk: &Disk) -> (String, String, Option<Tone>) {
    let clean = |s: &str| sanitise(s, 128);
    let opt = |s: &Option<String>| s.as_deref().map(clean);
    let what = [opt(&disk.transport), opt(&disk.model)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let (state, tone) = if !disk.installable() {
        (
            format!(
                "in use ({}), not installable",
                disk.mounts
                    .iter()
                    .map(|m| clean(m))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(Tone::Dim),
        )
    } else if disk.has_data() {
        let mut found: Vec<String> = disk.partition_table.iter().map(|t| clean(t)).collect();
        found.extend(disk.contents.iter().map(|c| {
            let fs = opt(&c.fstype).unwrap_or_else(|| "partition".into());
            match opt(&c.label).or_else(|| opt(&c.partlabel)) {
                Some(label) => format!("{fs} \"{label}\""),
                None => fs,
            }
        }));
        let earlier = if disk.formatted_this_boot {
            ", from an earlier attempt"
        } else {
            ""
        };
        (
            format!("HAS DATA: {}{earlier}", found.join(", ")),
            Some(Tone::Warn),
        )
    } else {
        ("empty".to_string(), None)
    };
    let what = format!(
        "{} {} {what}",
        clean(&disk.name),
        human_size(disk.size_bytes)
    );
    (what.trim_end().to_string(), state, tone)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    // Display only: precision loss above 2^53 bytes is irrelevant.
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64 / 1024.0;
    let mut unit = "KiB";
    for next in UNITS.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

/// Dials discovered targets, at most once per target and code, but again
/// when new addresses show up after a failed connection attempt.
#[derive(Default)]
struct Dialer {
    attempted: HashMap<EndpointId, BTreeSet<TransportAddr>>,
    /// Targets we ran pairing with, successfully or not.
    done: HashSet<EndpointId>,
    /// Spent guesses at the current code.
    spent: u32,
    /// Why the last connection attempt failed, for error messages.
    last_connect_error: Option<String>,
}

/// Outcome of trying one discovered target.
enum Attempt {
    Paired(Connection, Stream),
    Skipped,
    Failed,
    TooManyFailures,
}

impl Dialer {
    /// Whether `peer` is new, or has addresses we haven't tried yet.
    fn would_dial(&self, peer: &Peer) -> bool {
        !self.done.contains(&peer.id)
            && self
                .attempted
                .get(&peer.id)
                .is_none_or(|tried| !net::routes(peer).iter().all(|a| tried.contains(a)))
    }

    async fn attempt(
        &mut self,
        endpoint: &Endpoint,
        peer: &Peer,
        side: Side,
        code: &Code,
    ) -> Attempt {
        if !self.would_dial(peer) {
            return Attempt::Skipped;
        }
        self.attempted
            .entry(peer.id)
            .or_default()
            .extend(net::routes(peer));
        info!(target = %peer.id, label = %peer.label, addrs = ?net::dial_addrs(&peer.addrs), relayed = !peer.relays.is_empty(), "dialing target");
        match net::dial_and_pair(endpoint, peer, side, code).await {
            Ok((conn, stream)) => {
                self.done.insert(peer.id);
                Attempt::Paired(conn, stream)
            }
            Err(DialError::Connect(e)) => {
                warn!(target = %peer.id, "could not connect: {e:#}");
                let tried: Vec<String> = net::routes(peer)
                    .iter()
                    .map(|route| match route {
                        TransportAddr::Ip(addr) => addr.to_string(),
                        TransportAddr::Relay(url) => format!("relay {url}"),
                        other => format!("{other:?}"),
                    })
                    .collect();
                self.last_connect_error = Some(format!("{e:#} (tried {})", tried.join(", ")));
                Attempt::Failed
            }
            Err(DialError::Pair(failure)) => {
                warn!(target = %peer.id, label = %peer.label, "pairing failed: {}", failure.error);
                if failure.guess_spent {
                    self.done.insert(peer.id);
                    self.spent += 1;
                    if self.spent >= MAX_FAILURES {
                        return Attempt::TooManyFailures;
                    }
                } else if matches!(failure.error, PairError::Frame(_) | PairError::Timeout) {
                    // The connection broke before anything was revealed:
                    // safe to try again once other addresses show up.
                    self.last_connect_error = Some(failure.error.to_string());
                } else {
                    self.done.insert(peer.id);
                }
                Attempt::Failed
            }
        }
    }

    fn paired_with_any(&self) -> bool {
        !self.done.is_empty()
    }
}

/// How `nix-pixie controller serve` was invoked.
pub struct ServeArgs {
    pub identity: PathBuf,
    pub code_file: Option<PathBuf>,
    pub max_targets: Option<usize>,
    pub method: Method,
    /// Probe these hosts instead of searching the local network.
    pub targets: Vec<String>,
}

/// Alternative mode: show a controller code that operators type on targets,
/// and pair with every target that did. Prints one JSON object per target.
pub async fn serve(args: ServeArgs) -> Result<()> {
    // Controller codes have no nameplate to meet at, so this mode works on
    // local networks and with `--target` only.
    let targets = discovery::resolve_targets(&args.targets).await?;
    let secret = identity::load_or_create(&args.identity)?;
    let endpoint = net::bind(secret, 0, &Relays::Disabled).await?;
    let mut discovery = Discovery::start(args.method, targets).await?;
    info!(id = %endpoint.id(), "controller ready");

    let mut paired = Vec::new();
    let result = tokio::select! {
        r = pair_with_targets(&endpoint, &mut discovery, &args, &mut paired) => r,
        () = crate::shutdown_signal() => Ok(()),
    };
    for conn in paired {
        conn.close(VarInt::from_u32(0), b"done");
    }
    discovery.shutdown();
    endpoint.close().await;
    result
}

async fn pair_with_targets(
    endpoint: &Endpoint,
    discovery: &mut Discovery,
    args: &ServeArgs,
    paired: &mut Vec<Connection>,
) -> Result<()> {
    let mut code = new_code(args.code_file.as_deref())?;
    let mut dialer = Dialer::default();
    loop {
        if args.max_targets.is_some_and(|max| paired.len() >= max) {
            return Ok(());
        }
        let peer = discovery.next().await?;
        if peer.pairing != Pairing::TypedControllerCode || !dialer.would_dial(&peer) {
            continue;
        }
        let peer = discovery.settle(peer.id, SETTLE).await.unwrap_or(peer);
        match dialer.attempt(endpoint, &peer, Side::OwnsCode, &code).await {
            Attempt::Paired(conn, mut stream) => match introduction(&mut stream).await {
                Ok((label, inventory)) => {
                    let report = serde_json::json!({
                        "target": conn.remote_id().to_string(),
                        "label": label,
                        "addrs": peer.addrs.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        "inventory": inventory,
                    });
                    ui::print_result(&report.to_string());
                    paired.push(conn);
                }
                Err(e) => {
                    warn!(target = %peer.id, "target paired but did not introduce itself: {e:#}");
                }
            },
            Attempt::TooManyFailures => {
                warn!(
                    "{MAX_FAILURES} targets tried a wrong code; someone may be guessing. New code."
                );
                dialer.spent = 0;
                code = new_code(args.code_file.as_deref())?;
            }
            Attempt::Skipped | Attempt::Failed => {}
        }
    }
}

fn new_code(code_file: Option<&Path>) -> Result<Code> {
    let code = Code::generate(None)?;
    ui::note(&[
        String::new(),
        format!(
            "    Controller code:  {}",
            ui::paint(&code.to_string(), Tone::Bold)
        ),
        String::new(),
        format!("    On the target: nix-pixie agent --controller-code {code}"),
        String::new(),
    ]);
    if let Some(path) = code_file {
        write_private(path, &code.to_string())?;
    }
    Ok(code)
}
