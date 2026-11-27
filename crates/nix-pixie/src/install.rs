//! `nix-pixie controller install`: from a paired target to formatted disks
//! with the system on them.
//!
//! Everything that can fail without touching the target's disks happens
//! first: evaluating the configuration, choosing the disks, recording them,
//! building. Then the operator confirms what gets erased, and only then does
//! the target format its disks and receive the system.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nix_pixie_proto::ctrl::{
    ControllerMsg, DiskAssignment, KeyFile, Secret, Shutdown, StreamRequest, TargetMsg,
};
use nix_pixie_proto::inventory::Disk;
use tracing::{debug, info, trace, warn};
use zeroize::Zeroizing;

use crate::bootentry;
use crate::controller::{self, Approved, describe_disk, show_disk};
use crate::discovery::{Method, Peer};
use crate::exit;
use crate::facter;
use crate::hooks::{self, Step};
use crate::hostkey::{self, PublicKey};
use crate::link::Link;
use crate::metadata::{self, BootCheck, HostKeyMode, Machine, Metadata};
use crate::net::{self, Relays, Route, sanitise};
use crate::plan::{self, Choice, Resolution};
use crate::preflight;
use crate::push;
use crate::secrets;
use crate::system::{Boot, Built, Configuration, Disko, Facts, Ssh};
use crate::target::Target;
use crate::ui;

/// For the agent's checks before formatting (a fresh lsblk).
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
/// Unmounting and exporting pools.
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);
/// For the target to list its addresses.
const ADDRESSES_TIMEOUT: Duration = Duration::from_secs(10);
/// For the target to make its boot entry (efibootmgr, a few times).
const BOOT_ENTRY_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of disko's and nixos-install's output to keep for showing a
/// failure.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
const OUTPUT_TAIL_LINES: usize = 40;

/// How `nix-pixie controller install` was invoked.
pub struct InstallArgs {
    pub identity: PathBuf,
    /// `None`: asked for on the terminal, right before pairing, unless
    /// there is a controller's code.
    pub code: Option<String>,
    /// A controller's code to pair with instead.
    pub controller_code_file: Option<PathBuf>,
    /// A serial port to hand the target a controller's code through: ours
    /// from `controller_code_file`, or a new one.
    pub serial: Option<PathBuf>,
    /// Which of many devices of one configuration this is: for `{id}` in
    /// the machine's paths, and for hooks.
    pub id: Option<String>,
    /// How to check the first boot, instead of the machine's `bootCheck`.
    pub boot_check: Option<BootCheck>,
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
    /// How often to ask it meanwhile.
    pub boot_check_interval: Duration,
    /// Probe these hosts instead of searching the local network.
    pub targets: Vec<String>,
    /// Where to meet targets on other networks.
    pub relays: Relays,
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
    exit::reached(exit::Phase::Checking);
    if let Some(id) = &args.id {
        metadata::check_id(id)?;
    }
    // A typo in a given code fails at once.
    if let Some(code) = &args.code {
        controller::target_code(code)?;
    }
    if let Some(path) = &args.controller_code_file {
        controller::controller_code_file(path)?;
    }
    let requested = parse_disk_args(&args.disks)?;
    ui::steps(step_list(&args, Known::default()));
    // Check what we can before pairing: a mistake shouldn't cost the code.
    let flake = metadata::local_flake_dir(&args.flake)
        .map_or_else(|| args.flake.clone(), |dir| dir.display().to_string());
    let step = ui::start_with("check", flake);
    let metadata = metadata::load(&args.flake).await?;
    let programs = if let Some(name) = &args.machine {
        let machine = machine(&metadata, &args.flake, name)?;
        ui::steps(step_list(&args, Known::machine(machine)));
        let programs = check_machine(&args, name, machine, &requested).await?;
        step.done(format!("machine {name}"));
        Some(programs)
    } else {
        step.done(format!("{} machines", metadata.machines.len()));
        None
    };

    let code = controller::pair_code(
        args.code.as_deref(),
        args.controller_code_file.as_deref(),
        args.serial.is_some(),
        args.method,
        &args.targets,
    )
    .await?;
    let search = controller::Search {
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
            let (conn, stream, peer) = controller::pair_with_target(
                &endpoint,
                &mut discovery,
                code,
                args.timeout,
                args.serial.as_deref(),
            )
            .await?;
            exit::reached(exit::Phase::Paired);
            step.done(controller::describe_peer(&conn, &peer));
            let link = Link::new(&endpoint, &peer, conn, stream);
            let result = session(&link, &peer, &metadata, &args, requested, programs).await;
            // Closing is what lets the target shut down.
            link.close().await;
            result
        } => r,
        () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
    };
    discovery.shutdown();
    endpoint.close().await;
    let Installed {
        mut report,
        after,
        mut hooks,
    } = result?;
    let done = match after {
        After::Ssh(check) => tokio::select! {
            r = async {
                verify(check, &args, &mut report).await?;
                hooks.after(Step::Boot, None).await?;
                no_files_left(&mut hooks, Step::Boot)
            } => r,
            () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
        },
        After::Hooks(check) => tokio::select! {
            r = boot_by_hooks(&check, &args, &mut hooks, &mut report) => r,
            () = crate::shutdown_signal() => Err(anyhow!("interrupted")),
        },
        After::Nothing => Ok(()),
    };
    report.insert("hooks".into(), hooks.report());
    ui::finish();
    ui::print_result(&serde_json::Value::Object(report).to_string());
    done
}

/// What decides which steps an install has, as far as it is known.
#[derive(Debug, Default, Clone, Copy)]
struct Known<'a> {
    /// The machine being installed: its optional steps and its hooks.
    machine: Option<&'a Machine>,
    /// Whether the configuration has an SSH server to check after the first
    /// boot, once it is evaluated.
    ssh: Option<bool>,
    /// Whether the layout reads disk keys.
    keys: bool,
}

impl<'a> Known<'a> {
    fn machine(machine: &'a Machine) -> Self {
        Self {
            machine: Some(machine),
            ..Self::default()
        }
    }
}

/// The steps of the install, with each hook after the step it follows.
/// Refined as more is known; steps that turn out not to apply after all
/// (no address to check the first boot at) are skipped when that shows.
fn step_list(args: &InstallArgs, known: Known<'_>) -> Vec<(String, String)> {
    let machine = known.machine;
    let schedule = machine
        .and_then(|m| hooks::schedule(&m.hooks).ok())
        .unwrap_or_default();
    let configuration = machine.map_or_else(
        || "the configuration".to_string(),
        |m| format!("nixosConfigurations.{}", m.configuration),
    );
    let mut list = StepList {
        steps: Vec::new(),
        schedule: &schedule,
    };
    list.add("check", "Check the flake");
    list.add("pair", "Pair with the target");
    list.add("approve", "Approve the target");
    list.hooks(Step::Approve);
    if machine.is_some_and(|m| m.facter.report_path.is_some()) {
        list.add("facter", "Get the facter report");
    }
    list.hooks(Step::Facter);
    match machine.map(|m| m.host_key.mode) {
        Some(HostKeyMode::Pull) => list.add("host-key", "Pull the host key"),
        Some(HostKeyMode::Push) => list.add("host-key", "Push the host key"),
        Some(HostKeyMode::None) | None => {}
    }
    list.hooks(Step::HostKey);
    list.add("evaluate", &format!("Evaluate {configuration}"));
    list.add("disks", "Choose the disks");
    list.hooks(Step::Disks);
    if known.keys {
        list.add("keys", "Get the disk keys");
    }
    list.add("build", &format!("Build {configuration}"));
    list.hooks(Step::Build);
    list.add("format", "Format the disks");
    list.hooks(Step::Format);
    list.add("copy", "Send the system");
    list.hooks(Step::Copy);
    list.add("install", "Install the boot loader");
    list.hooks(Step::Install);
    let finish = match args.then {
        Then::Reboot => Some("Unmount and reboot"),
        Then::Poweroff => Some("Unmount and power off"),
        Then::Stay => None,
    };
    if let Some(finish) = finish {
        list.add("finish", finish);
        list.hooks(Step::Finish);
        let check = args
            .boot_check
            .or(machine.map(|m| m.boot_check))
            .unwrap_or_default();
        let boot = match check {
            BootCheck::Ssh => known.ssh != Some(false),
            BootCheck::Hooks => true,
            BootCheck::Nothing => false,
        };
        if boot {
            list.add("boot", "Wait for the first boot");
            list.hooks(Step::Boot);
        }
    }
    list.steps
}

/// The list [`step_list`] builds: keys and labels for [`ui::steps`].
struct StepList<'a> {
    steps: Vec<(String, String)>,
    schedule: &'a hooks::Schedule,
}

impl StepList<'_> {
    fn add(&mut self, key: &str, label: &str) {
        self.steps.push((key.to_string(), label.to_string()));
    }

    /// The hooks that run after `step`, whether or not it is in the list.
    fn hooks(&mut self, step: Step) {
        for hook in self.schedule.after(step) {
            self.steps
                .push((hooks::step_key(hook), format!("Run hook {hook}")));
        }
    }
}

/// Checks what can be checked about `machine` before pairing, and resolves
/// and builds its hooks.
async fn check_machine(
    args: &InstallArgs,
    name: &str,
    machine: &Machine,
    requested: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, PathBuf>> {
    check_push(name, machine)?;
    crate::encrypt::check_recipients(&machine.secrets.recipients)?;
    if args.boot_check.unwrap_or(machine.boot_check) == BootCheck::Hooks
        && hooks::schedule(&machine.hooks)?
            .after(Step::Boot)
            .is_empty()
    {
        bail!(
            "{name} checks its first boot with hooks (bootCheck = \"hooks\"), but no hook runs \
             after `boot`"
        );
    }
    if let Some(report) = missing_facter_report(&args.flake, machine, args.id.as_deref()) {
        // A configuration that reads its facter report can't evaluate
        // before the first one arrives (after approval); check it then.
        info!(
            report,
            "not evaluating nixosConfigurations.{} before pairing: its facter report isn't there \
             yet",
            machine.configuration
        );
    } else {
        ui::progress(format!(
            "evaluating nixosConfigurations.{}",
            machine.configuration
        ));
        let disko = disko_of(&Configuration::new(&args.flake, &machine.configuration)?).await?;
        let pinned = machine.disks.file.is_some();
        plan::check_links(&disko, pinned, &machine.configuration)?;
        plan::check_requested(&disko, pinned, requested)?;
        let has_hooks = !machine.hooks.is_empty();
        secrets::check_available(&secrets::needs(&disko.encryption)?, has_hooks)?;
    }
    if let Some(file) = &machine.disks.file {
        if file.contains(metadata::ID) {
            bail!(
                "disks.file ({file}) can't depend on the id: the configuration reads one file. \
                 For many devices, choose their disks with disks.rules"
            );
        }
        repo_root(&args.flake)?;
    }
    hooks::schedule(&machine.hooks)?;
    if !machine.hooks.is_empty() {
        ui::progress("building the hooks");
    }
    hooks::resolve(&args.flake, &machine.hooks).await
}

/// Runs the hooks after `step`, which can run programs on the target.
/// `installed`: where the installed system is mounted, once it is installed.
async fn hooks_after(
    hooks: &mut hooks::Runner,
    step: Step,
    link: &Link,
    installed: Option<&str>,
) -> Result<()> {
    hooks.after(step, Some(Target { link, installed })).await
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
    after: After,
    /// For the hooks after `boot`.
    hooks: hooks::Runner,
}

/// What checks the first boot.
enum After {
    /// Nothing: no reboot, or nothing to check it with.
    Nothing,
    /// The installed system's SSH server, asked for its host key.
    Ssh(Check),
    /// The hooks after `boot` (`bootCheck = "hooks"`).
    Hooks(Check),
}

/// Where and what to ask the installed system's SSH server.
struct Check {
    addrs: Vec<SocketAddr>,
    port: u16,
    /// Pull or push mode: the key the install put there.
    expected: Option<PublicKey>,
    /// The installer's own key, which may still answer for a moment.
    live: Option<PublicKey>,
    machine: String,
    /// The first boot may wait for a disk passphrase on the console.
    encrypted: bool,
    /// The first boot enrolls the Secure Boot keys: `Some(true)` if it
    /// reboots right after for that.
    enrolls_keys: Option<bool>,
}

impl Check {
    /// The check with the addresses to ask, if there are any: `ssh_at` is
    /// [`ssh_addrs`]' answer, or `None` without an SSH server. Otherwise the
    /// reason goes to the operator and into `report`.
    fn at(
        mut self,
        ssh_at: Option<Result<Vec<SocketAddr>, String>>,
        hooks: &hooks::Runner,
        report: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Option<Self> {
        let mut entry = serde_json::Map::new();
        match ssh_at {
            Some(Ok(addrs)) => {
                self.addrs = addrs;
                return Some(self);
            }
            Some(Err(reason)) => {
                let mut lines = vec![format!(
                    "Not waiting for {} to boot: {reason}.",
                    self.machine
                )];
                if let Some(key) = &self.expected {
                    lines.push(format!(
                        "Its host key, to compare when you first connect: {key}"
                    ));
                    entry.insert("expected".into(), key.to_string().into());
                }
                let after = hooks.scheduled_after(Step::Boot);
                if !after.is_empty() {
                    lines.push(format!(
                        "Not running the hooks after `boot`: {}",
                        after.join(", ")
                    ));
                }
                ui::show(lines);
                ui::skip("boot", "no address to ask");
                for hook in after {
                    ui::skip(&hooks::step_key(hook), "no boot to wait for");
                }
                entry.insert("skipped".into(), reason.into());
            }
            None => {
                entry.insert("skipped".into(), "services.openssh is not enabled".into());
            }
        }
        report.insert("hostKey".into(), entry.into());
        None
    }
}

/// The first boot, checked by the hooks after `boot`: they wait for it
/// their own way, and may report the host key they saw, which then must be
/// the expected one. Records the result in `report`.
async fn boot_by_hooks(
    check: &Check,
    args: &InstallArgs,
    hooks: &mut hooks::Runner,
    report: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let step = ui::start("boot");
    ui::show(boot_notice(check, args));
    hooks.context.expected_host_key = check.expected.as_ref().map(ToString::to_string);
    hooks.context.boot_timeout_secs = Some(args.boot_timeout.as_secs());
    let ran = async {
        hooks.after(Step::Boot, None).await?;
        no_files_left(hooks, Step::Boot)
    }
    .await;
    if let Err(e) = ran {
        report.insert(
            "hostKey".into(),
            serde_json::json!({ "verified": false, "through": "hooks", "error": format!("{e:#}") }),
        );
        return Err(e);
    }
    let seen = hooks
        .take_seen_host_key()
        .map(|key| PublicKey::parse(&key).context("the host key a hook saw (seen-host-key)"))
        .transpose()?;
    let (entry, done) = match (&seen, &check.expected) {
        (Some(seen), Some(expected)) if seen != expected => {
            report.insert(
                "hostKey".into(),
                serde_json::json!({ "verified": false, "through": "hooks", "seen": seen.to_string() }),
            );
            bail!(
                "HOST KEY MISMATCH: a hook saw {seen} on {}, but the install put {expected} there",
                check.machine
            );
        }
        (Some(seen), Some(_)) => (
            serde_json::json!({ "verified": true, "through": "hooks", "seen": seen.to_string() }),
            "up, host key verified".to_string(),
        ),
        (Some(seen), None) => (
            serde_json::json!({ "verified": false, "through": "hooks", "seen": seen.to_string() }),
            "up, new host key".to_string(),
        ),
        (None, _) => (
            serde_json::json!({ "verified": false, "through": "hooks" }),
            "up, as the hooks say".to_string(),
        ),
    };
    report.insert("hostKey".into(), entry);
    step.done(done);
    Ok(())
}

/// Waits for the installed system and compares its host key; records the
/// result in `report`.
async fn verify(
    check: Check,
    args: &InstallArgs,
    report: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let step = ui::start("boot");
    ui::show(boot_notice(&check, args));
    let result = wait_for_boot(&check, args, &step).await;
    let verified = check.expected.is_some();
    let entry = match &result {
        Ok(seen) => {
            ui::show(vec![if verified {
                format!(
                    "{} is up, with the host key the install put there.",
                    check.machine
                )
            } else {
                format!(
                    "{} is up. Its host key is new, so it can't be checked: {}",
                    check.machine, seen.key
                )
            }]);
            serde_json::json!({
                "verified": verified,
                "seen": seen.key.to_string(),
                "address": seen.addr.to_string(),
            })
        }
        Err(e) => {
            // The error says what happened; the notice is stale.
            ui::show(Vec::new());
            serde_json::json!({ "verified": false, "error": format!("{e:#}") })
        }
    };
    report.insert("hostKey".into(), entry);
    let seen = result?;
    let key = if verified {
        "host key verified"
    } else {
        "new host key"
    };
    step.done(format!("up at {}, {key}", seen.addr));
    Ok(())
}

/// What the operator learns while the controller waits for the first boot.
fn boot_notice(check: &Check, args: &InstallArgs) -> Vec<String> {
    let mut lines = vec![if args.then == Then::Poweroff {
        format!(
            "{} is powered off. Remove the installer medium if needed and power it on.",
            check.machine
        )
    } else {
        format!("{} reboots into the installed system.", check.machine)
    }];
    if check.encrypted {
        lines.push(
            "Its disks are encrypted: it may ask for the passphrase on its console while \
             booting; type it there."
                .into(),
        );
    }
    match check.enrolls_keys {
        Some(true) => lines.push(
            "On its first boot it prepares its Secure Boot keys and reboots by itself, for \
             systemd-boot to enroll them."
                .into(),
        ),
        Some(false) => lines.push(
            "On its first boot it prepares its Secure Boot keys; systemd-boot enrolls them \
             when it next boots."
                .into(),
        ),
        None => {}
    }
    let pace = if args.boot_check_interval < args.boot_timeout {
        format!(
            "Checking for it every {}, for up to {}",
            ui::span(args.boot_check_interval),
            ui::span(args.boot_timeout)
        )
    } else {
        format!("Checking for it in {}", ui::span(args.boot_timeout))
    };
    lines.push(if ui::interactive() {
        format!("{pace}. Press Enter once it is up to check right away.")
    } else {
        format!("{pace}.")
    });
    lines
}

/// Asks the installed system for its host key at the boot check interval,
/// and whenever the operator presses Enter, until it answers or the boot
/// timeout passes.
async fn wait_for_boot(
    check: &Check,
    args: &InstallArgs,
    step: &ui::Running,
) -> Result<hostkey::Seen> {
    let hosts = hostkey::hosts(&check.addrs);
    let at = hostkey::describe_hosts(&hosts);
    let mut cadence =
        hostkey::Cadence::new(Instant::now(), args.boot_check_interval, args.boot_timeout);
    let mut enter = ui::interactive();
    let mut checks = 0_u32;
    while let Some(next) = cadence.next() {
        let asked = wait_until(next, &mut enter, step, checks).await;
        checks += 1;
        info!(
            check = checks,
            %at,
            port = check.port,
            asked,
            "checking whether {} is up",
            check.machine
        );
        step.detail("asking for its host key");
        let seen = hostkey::check(
            &hosts,
            check.port,
            check.expected.as_ref(),
            check.live.as_ref(),
        )
        .await?;
        if let Some(seen) = seen {
            return Ok(seen);
        }
        debug!(check = checks, "no answer yet");
        cadence.checked(Instant::now());
    }
    bail!(
        "no SSH answer from {at} within {}",
        ui::span(args.boot_timeout)
    )
}

/// Waits until `next`, counting down on `step`, or until the operator
/// presses Enter (then `true`). Stops listening for Enter once stdin ended
/// for good.
async fn wait_until(next: Instant, enter: &mut bool, step: &ui::Running, checks: u32) -> bool {
    let deadline = tokio::time::Instant::from_std(next);
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return false,
            line = ui::line(), if *enter => match line {
                Some(_) => return true,
                None => *enter = !ui::input_ended(),
            },
            _ = clock.tick() => {
                let left = next.saturating_duration_since(Instant::now()).as_millis().div_ceil(1000);
                step.detail(if checks == 0 {
                    format!("first check in {left} s")
                } else {
                    format!("no answer yet · next check in {left} s")
                });
            }
        }
    }
}

/// The machine's facter report path, if it wants one and the repository
/// doesn't have it yet (a first install).
fn missing_facter_report(flake: &str, machine: &Machine, id: Option<&str>) -> Option<String> {
    let report = machine.facter.report_path.as_deref()?;
    // Without the id yet, it can't be told: as if it were missing.
    let Ok(report) = metadata::with_id(report, id, "facter.reportPath") else {
        return Some(report.to_string());
    };
    let root = metadata::local_flake_dir(flake)?;
    (!root.join(&report).exists()).then_some(report)
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
    link: &Link,
    peer: &Peer,
    metadata: &Metadata,
    args: &InstallArgs,
    requested: BTreeMap<String, String>,
    programs: Option<BTreeMap<String, PathBuf>>,
) -> Result<Installed> {
    let plan = prepare(link, peer, metadata, args, requested, programs).await?;
    apply(link, peer, args, plan).await
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
    boot: Boot,
    ssh: Ssh,
    /// Pull mode: the generated key, and where the installed system wants it.
    host_key: Option<(PublicKey, String)>,
    /// Passphrases disko reads from files while formatting.
    key_files: Vec<KeyFile>,
    hooks: hooks::Runner,
    boot_check: BootCheck,
    /// Whether the target boots with UEFI.
    uefi: bool,
}

/// Approval, the facter report and host key, the disk plan, the build.
async fn prepare(
    link: &Link,
    peer: &Peer,
    metadata: &Metadata,
    args: &InstallArgs,
    mut requested: BTreeMap<String, String>,
    programs: Option<BTreeMap<String, PathBuf>>,
) -> Result<Plan> {
    let step = ui::start("approve");
    let Approved {
        label,
        inventory,
        machine: name,
        ..
    } = controller::approve(
        link,
        peer,
        Some(metadata),
        args.machine.as_deref(),
        args.yes,
        true,
    )
    .await?;
    let name = name.context("approved without a machine")?;
    let machine = machine(metadata, &args.flake, &name)?;
    ui::steps(step_list(args, Known::machine(machine)));
    check_push(&name, machine)?;
    let target = serde_json::json!({
        "id": link.target().to_string(),
        "label": label,
        "inventory": inventory,
    });
    let mut hooks = start_hooks(args, &name, machine, target, programs).await?;
    step.done(name.clone());
    hooks_after(&mut hooks, Step::Approve, link, None).await?;
    facter_step(link, &args.flake, machine, &mut hooks).await?;
    hooks_after(&mut hooks, Step::Facter, link, None).await?;
    // Before building: the configuration (or a rekeying hook) may use it.
    let pulled = machine_host_key(link, &args.flake, &name, machine, &mut hooks).await?;
    hooks.context.host_key = pulled.as_ref().map(ToString::to_string);
    hooks_after(&mut hooks, Step::HostKey, link, None).await?;

    // A configuration that doesn't evaluate can be fixed meanwhile.
    let Evaluated {
        configuration,
        facts,
        disko,
        needs,
        host_key,
    } = ui::retrying(
        &format!("Evaluate nixosConfigurations.{}", machine.configuration),
        async || evaluate(args, machine, &inventory, pulled.clone()).await,
    )
    .await?;

    let step = ui::start("disks");
    let choices = choose_disks(&disko, machine, &inventory.disks, &mut requested, args.yes).await?;
    let formatting = choices.iter().map(|c| c.disk.as_str()).collect();
    plan::check_name_clashes(&plan::created_names(&disko), &formatting, &inventory.disks)
        .map_err(|reason| anyhow!(reason))?;
    if let Some(file) = &machine.disks.file {
        record_pins(&configuration, &args.flake, file, &choices).await?;
    }
    step.done(
        choices
            .iter()
            .map(|c| format!("{} = {}", c.role, c.disk))
            .collect::<Vec<_>>()
            .join(", "),
    );
    hooks.context.disks = Some(serde_json::to_value(&choices)?);
    hooks_after(&mut hooks, Step::Disks, link, None).await?;

    // Before the build, so it can run unattended.
    let step = (!needs.is_empty()).then(|| ui::start("keys"));
    let key_files = secrets::gather(needs, hooks.take_disk_keys()).await?;
    if let Some(step) = step {
        step.done(format!("{} key file(s)", key_files.len()));
    }

    let built = build(&configuration, &planned_facts(&facts, &choices)).await?;
    hooks.context.system = Some(built.toplevel.clone());
    hooks_after(&mut hooks, Step::Build, link, None).await?;
    Ok(Plan {
        label,
        name,
        configuration: machine.configuration.clone(),
        disks: inventory.disks,
        choices,
        built,
        disko,
        boot: facts.boot,
        ssh: facts.ssh,
        host_key,
        key_files,
        hooks,
        boot_check: args.boot_check.unwrap_or(machine.boot_check),
        uefi: machine.firmware.apply(&inventory.firmware).uefi,
    })
}

/// The `facter` step, if the machine wants a report.
async fn facter_step(
    link: &Link,
    flake: &str,
    machine: &Machine,
    hooks: &mut hooks::Runner,
) -> Result<()> {
    if let Some(path) = &machine.facter.report_path {
        let path = metadata::with_id(path, hooks.context.id.as_deref(), "facter.reportPath")?;
        let step = ui::start("facter");
        let facter = &machine.facter;
        let outcome = facter::fetch(link, flake, &path, facter.mode, &facter.ignore)
            .await
            .context("getting the facter report")?;
        match outcome {
            facter::Outcome::Written => step.done(path),
            facter::Outcome::Kept => step.done(format!("{path}, kept")),
            facter::Outcome::Compared(differences) if differences.is_empty() => {
                step.done(format!("{path}, the same"));
            }
            facter::Outcome::Compared(differences) => {
                warn!(
                    path,
                    ?differences,
                    "the target's hardware differs from the facter report"
                );
                ui::show(
                    std::iter::once(format!(
                        "This target's hardware differs from {path} in {} place(s):",
                        differences.len()
                    ))
                    .chain(differences.iter().map(|d| format!("  {d}")))
                    .collect(),
                );
                ui::commit();
                step.done(format!("{path}, {} difference(s)", differences.len()));
                hooks.context.facter_differences = Some(differences);
            }
        }
    }
    Ok(())
}

/// Builds the system and the disko script. A failed build can be fixed
/// meanwhile and tried again, as long as the fix leaves alone what the
/// install was `planned` for.
async fn build(configuration: &Configuration, planned: &Facts) -> Result<Built> {
    let mut again = false;
    ui::retrying(
        &format!("Build nixosConfigurations.{}", configuration.name()),
        async || {
            if again {
                check_unchanged(configuration, planned).await?;
            }
            again = true;
            let step = ui::start("build");
            let built = configuration.build().await?;
            step.done("");
            Ok(built)
        },
    )
    .await
}

/// The facts the install was planned with: as evaluated, with the disks
/// that were pinned since then.
pub fn planned_facts(facts: &Facts, choices: &[Choice]) -> Facts {
    let mut planned = facts.clone();
    if let Some(disko) = &mut planned.disko {
        for choice in choices {
            if let (Some(pin), Some(disk)) = (&choice.pin, disko.disks.get_mut(&choice.name)) {
                disk.device = Some(pin.clone());
            }
        }
    }
    planned
}

/// Before building again: the configuration may have changed since the
/// failed build (that's the point), but not in what the install was
/// planned for.
async fn check_unchanged(configuration: &Configuration, planned: &Facts) -> Result<()> {
    if configuration.facts().await? != *planned {
        bail!(
            "nixosConfigurations.{} changed in what the install was planned for (its disks, \
             boot loader, platform or SSH server); pair again to start over",
            configuration.name()
        );
    }
    Ok(())
}

/// What evaluating the approved machine's configuration settled.
struct Evaluated {
    configuration: Configuration,
    facts: Facts,
    disko: Disko,
    /// The key files its layout reads while formatting.
    needs: Vec<secrets::Need>,
    /// Pull mode: the pulled key, and where the installed system wants it.
    host_key: Option<(PublicKey, String)>,
}

/// Evaluates the approved machine's configuration and checks it against the
/// target, refining the steps with what it says.
async fn evaluate(
    args: &InstallArgs,
    machine: &Machine,
    inventory: &nix_pixie_proto::inventory::Inventory,
    pulled: Option<PublicKey>,
) -> Result<Evaluated> {
    let step = ui::start("evaluate");
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
    let firmware = machine.firmware.apply(&inventory.firmware);
    preflight::check_boot(&facts.boot, &firmware, &machine.configuration)?;
    preflight::check_boot_check(
        &facts.boot,
        &facts.ssh,
        args.boot_check.unwrap_or(machine.boot_check),
        &machine.configuration,
    )?;
    let host_key = host_key_path(pulled, &facts, &machine.configuration)?;
    let disko = facts
        .disko
        .clone()
        .with_context(|| no_disko(&machine.configuration))?;
    plan::check_links(&disko, machine.disks.file.is_some(), &machine.configuration)?;
    let needs = secrets::needs(&disko.encryption)?;
    secrets::check_available(&needs, !machine.hooks.is_empty())?;
    ui::steps(step_list(
        args,
        Known {
            machine: Some(machine),
            ssh: Some(facts.ssh.enable),
            keys: !needs.is_empty(),
        },
    ));
    step.done(facts.system.clone());
    Ok(Evaluated {
        configuration,
        facts,
        disko,
        needs,
        host_key,
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
    let mut runner = hooks::Runner::new(
        machine.hooks.clone(),
        programs,
        metadata::local_flake_dir(&args.flake).map_or_else(std::env::current_dir, Ok)?,
        hooks::Context {
            machine: name.to_string(),
            configuration: machine.configuration.clone(),
            flake: args.flake.clone(),
            target,
            id: args.id.clone(),
            ..hooks::Context::default()
        },
    )?;
    runner.encrypt_to(machine.secrets.recipients.clone());
    Ok(runner)
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
async fn apply(link: &Link, peer: &Peer, args: &InstallArgs, plan: Plan) -> Result<Installed> {
    let Plan {
        label,
        name,
        configuration,
        disks,
        choices,
        built,
        disko,
        boot,
        ssh,
        host_key,
        key_files,
        mut hooks,
        boot_check,
        uefi,
    } = plan;
    confirm_erase(&disks, &choices, args.yes, args.wipe).await?;
    format_disks(link, &choices, &disko, &built.disko_script, key_files).await?;
    let root = disko.root_mount_point;
    hooks.context.root = Some(root.clone());
    hooks_after(&mut hooks, Step::Format, link, None).await?;
    place_files(link, &root, &mut hooks).await?;

    let (fstype, copied) = send_system(link, &root, &built.toplevel).await?;
    hooks_after(&mut hooks, Step::Copy, link, None).await?;
    place_files(link, &root, &mut hooks).await?;
    let step = ui::start("install");
    let (expected, key_path) = host_key.unzip();
    install_system(link, &root, &built.toplevel, key_path).await?;
    step.done("");
    hooks_after(&mut hooks, Step::Install, link, Some(&root)).await?;
    place_files(link, &root, &mut hooks).await?;

    let mut report = report(
        link,
        label,
        &name,
        configuration,
        &choices,
        &built.toplevel,
        &root,
        fstype,
        copied.seconds,
        args.then,
    )?;
    if let Some(id) = &hooks.context.id {
        report.insert("id".into(), id.clone().into());
    }
    // A lease may have come since pairing (a NIC whose driver is slow).
    let addrs = own_addresses(link).await?;
    if !addrs.is_empty() {
        let shown: Vec<String> = addrs.iter().map(ToString::to_string).collect();
        info!(addresses = shown.join(", "), "the target's addresses now");
        report.insert("targetAddresses".into(), shown.into());
    }
    let boot_entry = bootentry::Plan::of(&boot, uefi, &root);
    let leaving = Leaving {
        name,
        root,
        zpools: disko.zpools,
        ssh,
        expected,
        boot_check,
        encrypted: !(disko.encryption.luks.is_empty() && disko.encryption.zfs.is_empty()),
        enrolls_keys: boot.enroll_keys.then_some(boot.enroll_reboots),
        boot_entry,
    };
    let after = leave(link, peer, args, &mut hooks, &mut report, leaving).await?;
    Ok(Installed {
        report,
        after,
        hooks,
    })
}

/// What leaving the target needs from the install.
struct Leaving {
    name: String,
    root: String,
    zpools: Vec<String>,
    ssh: Ssh,
    /// The host key the install put there.
    expected: Option<PublicKey>,
    boot_check: BootCheck,
    encrypted: bool,
    enrolls_keys: Option<bool>,
    /// What the firmware needs to boot the installed system next.
    boot_entry: bootentry::Plan,
}

/// Leaves the target ready to boot (or mounted, with `--then stay`), and
/// says what checks the first boot.
async fn leave(
    link: &Link,
    peer: &Peer,
    args: &InstallArgs,
    hooks: &mut hooks::Runner,
    report: &mut serde_json::Map<String, serde_json::Value>,
    leaving: Leaving,
) -> Result<After> {
    let Leaving {
        name,
        root,
        zpools,
        ssh,
        expected,
        boot_check,
        encrypted,
        enrolls_keys,
        boot_entry,
    } = leaving;
    add_boot_entry(link, &boot_entry, &name, args.then, report).await?;
    let shutdown = match args.then {
        Then::Stay => {
            ui::show(vec![format!(
                "Installed; {root} stays mounted on the target."
            )]);
            return Ok(After::Nothing);
        }
        Then::Reboot => Shutdown::Reboot,
        Then::Poweroff => Shutdown::PowerOff,
    };
    // Asked while the target still answers.
    let ssh_at = if ssh.enable && boot_check == BootCheck::Ssh {
        Some(ssh_addrs(link, peer).await?)
    } else {
        None
    };
    let step = ui::start("finish");
    let live = finish(link, &root, &zpools, shutdown).await?;
    exit::reached(exit::Phase::Installed);
    step.done(match shutdown {
        Shutdown::Reboot => "rebooting",
        Shutdown::PowerOff => "powering off",
    });
    hooks.context.root = None;
    hooks.after(Step::Finish, None).await?;
    no_files_left(hooks, Step::Finish)?;
    let check = Check {
        addrs: Vec::new(),
        port: ssh.ports.first().copied().unwrap_or(22),
        expected,
        live,
        machine: name,
        encrypted,
        enrolls_keys,
    };
    Ok(match boot_check {
        BootCheck::Ssh => check
            .at(ssh_at, hooks, report)
            .map_or(After::Nothing, After::Ssh),
        BootCheck::Hooks => After::Hooks(check),
        BootCheck::Nothing => {
            report.insert(
                "hostKey".into(),
                serde_json::json!({ "skipped": "bootCheck = none" }),
            );
            After::Nothing
        }
    })
}

/// Has the target's firmware boot the installed system next, if its boot
/// loader doesn't see to that, or says what to mind. Failing that isn't
/// failing the install: the system is there, and a reboot into the
/// installer only needs it taken out.
async fn add_boot_entry(
    link: &Link,
    plan: &bootentry::Plan,
    machine: &str,
    then: Then,
    report: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let poweroff = if then == Then::Reboot {
        ", or install with --then poweroff"
    } else {
        ""
    };
    let entry = match plan {
        bootentry::Plan::Nothing => return Ok(()),
        bootentry::Plan::NotAllowed => {
            ui::show(vec![format!(
                "lanzaboote makes no boot entry, and nixosConfigurations.{machine} doesn't let \
                 nix-pixie make one (boot.loader.efi.canTouchEfiVariables = false): the firmware \
                 boots whatever comes first in its boot order. If that is the installer, take \
                 it out before the machine reboots{poweroff}."
            )]);
            serde_json::json!({ "skipped": "canTouchEfiVariables = false" })
        }
        bootentry::Plan::Add { esp } => {
            let request = ControllerMsg::AddBootEntry { esp: esp.clone() };
            match link
                .request(&request, BOOT_ENTRY_TIMEOUT, "the boot entry")
                .await?
            {
                TargetMsg::BootEntry { number, created } => {
                    info!(
                        entry = %format!("Boot{number}"),
                        created,
                        "the firmware boots the installed system next"
                    );
                    serde_json::json!({ "number": number, "created": created })
                }
                TargetMsg::BootEntryFailed { error } => {
                    warn!("no boot entry for the installed system: {error}");
                    ui::show(vec![format!(
                        "No boot entry for the installed system ({error}). lanzaboote makes \
                         none, so the firmware boots whatever comes first in its boot order: if \
                         that is the installer, take it out before the machine reboots{poweroff}."
                    )]);
                    serde_json::json!({ "failed": error })
                }
                _ => bail!("unexpected reply to the boot entry"),
            }
        }
    };
    report.insert("bootEntry".into(), entry);
    Ok(())
}

/// Formats the confirmed disks: sends the disko script, has the target
/// check its disks once more, and runs disko there.
async fn format_disks(
    link: &Link,
    choices: &[Choice],
    disko: &Disko,
    script: &str,
    key_files: Vec<KeyFile>,
) -> Result<()> {
    let step = ui::start_with("format", "sending the disko script");
    push::send(
        link,
        &StreamRequest::LiveStore,
        std::slice::from_ref(&script.to_string()),
    )
    .await
    .context("pushing the disko script into the target's live store")?;
    step.detail("checking the disks on the target");
    let labels = plan::created_names(disko).into_iter().collect();
    prepare_disks(link, choices, labels).await?;
    step.detail("running disko");
    run_disko(link, script, key_files).await?;
    step.done(format!("mounted at {}", disko.root_mount_point));
    Ok(())
}

/// Copies the system into the store under `root` on the target. Returns
/// that store's filesystem type and what was copied.
async fn send_system(link: &Link, root: &str, toplevel: &str) -> Result<(String, push::Copied)> {
    let fstype = push::prepare_disk_store(link, root).await?;
    let into = format!("into {root}, {fstype}");
    let step = ui::start_with("copy", into.clone());
    push::warn_if_relayed(&link.connection());
    let copied = push::send(
        link,
        &StreamRequest::Store { root: root.into() },
        std::slice::from_ref(&toplevel.to_string()),
    )
    .await?;
    step.done(if copied.what.is_empty() {
        into
    } else {
        format!("{} {into}", copied.what)
    });
    Ok((fstype, copied))
}

/// The result so far, as printed at the end.
#[allow(clippy::too_many_arguments, reason = "the result's fields, named")]
fn report(
    link: &Link,
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
        ("target", link.target().to_string().into()),
        ("label", label.into()),
        ("machine", name.into()),
        ("configuration", configuration.into()),
        ("disks", serde_json::to_value(choices)?),
        ("system", system.into()),
        ("root", root.into()),
        ("fstype", fstype.into()),
        ("pushSeconds", seconds.into()),
        ("route", net::route(&link.connection()).kind().into()),
        ("then", format!("{then:?}").to_ascii_lowercase().into()),
    ] {
        report.insert(key.into(), value);
    }
    Ok(report)
}

/// Puts the files hooks wrote so far into the installed system.
async fn place_files(link: &Link, root: &str, hooks: &mut hooks::Runner) -> Result<()> {
    let files = hooks.take_files();
    if files.is_empty() {
        return Ok(());
    }
    info!(
        files = files.len(),
        "putting the hooks' files into the installed system"
    );
    let request = ControllerMsg::PlaceFiles {
        root: root.into(),
        files,
    };
    let reply = link
        .request(&request, PREPARE_TIMEOUT, "the target to place the files")
        .await?;
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
    link: &Link,
    flake: &str,
    name: &str,
    machine: &Machine,
    hooks: &mut hooks::Runner,
) -> Result<Option<PublicKey>> {
    let pushed = hooks.take_host_key();
    let id = hooks.context.id.clone();
    let id = id.as_deref();
    Ok(match (machine.host_key.mode, pushed) {
        (HostKeyMode::None, None) => None,
        (HostKeyMode::Pull, None) => Some(host_key(link, flake, name, machine, id, None).await?),
        (HostKeyMode::Push, Some(private)) => {
            Some(host_key(link, flake, name, machine, id, Some(private)).await?)
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
    link: &Link,
    flake: &str,
    name: &str,
    machine: &Machine,
    id: Option<&str>,
    pushed: Option<Zeroizing<Vec<u8>>>,
) -> Result<PublicKey> {
    let how = if pushed.is_some() { "pushed" } else { "pulled" };
    let step = ui::start("host-key");
    let request = match pushed {
        Some(private) => ControllerMsg::ProvideHostKey {
            private: Secret(private),
        },
        None => ControllerMsg::GenerateHostKey,
    };
    let reply = link
        .request(&request, PREPARE_TIMEOUT, "the target's host key")
        .await?;
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
        let path = &metadata::with_id(path, id, "hostKey.publicKeyPath")?;
        // Many devices of one machine: each by its id.
        let name = id.unwrap_or(name);
        let root = repo_root(flake)?;
        let file = metadata::repo_path(&root, path)?;
        let entry = key.to_line(Some(&format!("root@{name}")));
        metadata::write_into_repo(&root, &file, entry.as_bytes()).await?;
        info!(%path, "recorded the host key");
    }
    step.done(key.short());
    Ok(key)
}

/// Sends a request whose reply follows `Output` frames, and keeps the end
/// of that output.
async fn follow(link: &Link, request: &ControllerMsg) -> Result<(TargetMsg, Tail)> {
    let mut tail = Tail::default();
    let reply = link
        .request_with_output(request, async |frame| {
            if let TargetMsg::Output { chunk } = frame {
                trace!(bytes = chunk.len(), "output");
                tail.push(&chunk);
            }
        })
        .await?;
    Ok((reply, tail))
}

async fn install_system(
    link: &Link,
    root: &str,
    system: &str,
    host_key_path: Option<String>,
) -> Result<()> {
    let request = ControllerMsg::Install {
        root: root.into(),
        system: system.into(),
        host_key_path,
    };
    match follow(link, &request).await? {
        (TargetMsg::InstallDone, _) => Ok(()),
        (TargetMsg::InstallFailed { error }, tail) => {
            ui::note(&[tail.lines(OUTPUT_TAIL_LINES)]);
            bail!("installing failed: {}", sanitise(&error, 300))
        }
        _ => bail!("unexpected message while installing"),
    }
}

/// Where the installed system's SSH server can be asked for its host key,
/// or why nowhere.
///
/// A target found on a network, or named with `--target`, answers where it
/// was found. One found only at a relay has no address we know to be its
/// own: a direct path to it may end at a NAT, and whatever answers SSH at a
/// NAT's address is another machine. So the target is asked.
async fn ssh_addrs(link: &Link, peer: &Peer) -> Result<Result<Vec<SocketAddr>, String>> {
    if !peer.addrs.is_empty() {
        return Ok(Ok(peer.addrs.iter().copied().collect()));
    }
    let Route::Direct(addr) = net::route(&link.connection()) else {
        return Ok(Err(
            "it was only reached through a relay, so there is no address to ask for its host key"
                .into(),
        ));
    };
    let own = own_addresses(link).await?;
    Ok(if net::is_own(addr, &own) {
        Ok(vec![addr])
    } else {
        Err(format!(
            "it is behind NAT ({} is not an address of its own), so its SSH server can't be \
             asked for its host key from here",
            addr.ip()
        ))
    })
}

/// The addresses of the target's network interfaces.
async fn own_addresses(link: &Link) -> Result<Vec<IpAddr>> {
    let reply = link
        .request(
            &ControllerMsg::RequestAddresses,
            ADDRESSES_TIMEOUT,
            "the target's addresses",
        )
        .await?;
    match reply {
        TargetMsg::Addresses { addrs } => Ok(addrs),
        _ => bail!("unexpected reply to asking for the target's addresses"),
    }
}

/// Unmounts and exports on the target. Returns the installer's own host key.
async fn finish(
    link: &Link,
    root: &str,
    zpools: &[String],
    then: Shutdown,
) -> Result<Option<PublicKey>> {
    let request = ControllerMsg::Finish {
        root: root.into(),
        zpools: zpools.to_vec(),
        then,
    };
    let reply = link
        .request(&request, FINISH_TIMEOUT, "the target to unmount")
        .await?;
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
    machine: &Machine,
    present: &[Disk],
    requested: &mut BTreeMap<String, String>,
    yes: bool,
) -> Result<Vec<Choice>> {
    let slots = plan::slots(disko, machine.disks.file.is_some(), present)?;
    loop {
        let resolved = plan::resolve(&slots, present, requested, &machine.disks.rules)?;
        let (role, candidates) = match resolved {
            Resolution::Done(choices) => return Ok(choices),
            Resolution::Ask { role, candidates } => (role, candidates),
        };
        let offered: Vec<&Disk> = candidates
            .iter()
            .filter_map(|name| present.iter().find(|d| &d.name == name))
            .collect();
        if yes || !ui::interactive() {
            let described: Vec<String> = offered.iter().map(|d| describe_disk(d)).collect();
            bail!(
                "choose the disk for {role} with --disk {role}=<disk>. Candidates:\n  {}",
                described.join("\n  ")
            );
        }
        let mut lines = vec![format!("Which disk is {role}?")];
        for (i, disk) in offered.iter().enumerate() {
            lines.push(format!("  {}) {}", i + 1, show_disk(disk)));
        }
        ui::show(lines);
        let answer = ui::ask("Number: ").await.unwrap_or_default();
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
/// The operator approved the machine already, so a `y` is enough here too;
/// without a terminal, data needs `--wipe` on top of `--yes`.
async fn confirm_erase(disks: &[Disk], choices: &[Choice], yes: bool, wipe: bool) -> Result<()> {
    let chosen: Vec<(&Choice, &Disk)> = choices
        .iter()
        .filter_map(|c| Some((c, disks.iter().find(|d| d.name == c.disk)?)))
        .collect();
    let with_data = chosen.iter().any(|(_, d)| d.has_data());
    let mut lines = vec!["This erases everything on:".to_string()];
    for (choice, disk) in &chosen {
        lines.push(format!("  {:<10} {}", choice.role, show_disk(disk)));
    }
    ui::show(lines);
    let (question, refusal) = if with_data {
        if wipe {
            ui::commit();
            return Ok(());
        }
        if yes || !ui::interactive() {
            ui::commit();
            bail!("there is data on these disks; pass --wipe to erase it");
        }
        (
            "Erase these disks and the data on them? [y/N] ",
            "not erasing",
        )
    } else {
        if yes {
            ui::commit();
            return Ok(());
        }
        ("Format these disks? [y/N] ", "not formatting")
    };
    if !ui::confirm(question).await {
        bail!(refusal);
    }
    Ok(())
}

async fn prepare_disks(link: &Link, choices: &[Choice], labels: Vec<String>) -> Result<()> {
    let disks = choices
        .iter()
        .map(|c| DiskAssignment {
            role: c.role.clone(),
            device: c.disk.clone(),
            link: c.link,
        })
        .collect();
    let request = ControllerMsg::PrepareDisks { disks, labels };
    let reply = link
        .request(&request, PREPARE_TIMEOUT, "the target to check its disks")
        .await?;
    match reply {
        TargetMsg::DisksReady => Ok(()),
        TargetMsg::DisksRefused { reason } => {
            bail!("the target refused to format: {}", sanitise(&reason, 300))
        }
        _ => bail!("unexpected reply to the disk check"),
    }
}

/// Runs disko on the target. Its output is kept, and shown if it fails.
async fn run_disko(link: &Link, script: &str, key_files: Vec<KeyFile>) -> Result<()> {
    exit::reached(exit::Phase::Erasing);
    let request = ControllerMsg::RunDisko {
        script: script.into(),
        key_files,
    };
    match follow(link, &request).await? {
        (TargetMsg::DiskoDone, _) => Ok(()),
        (TargetMsg::DiskoFailed { error }, tail) => {
            ui::note(&[tail.lines(OUTPUT_TAIL_LINES)]);
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
