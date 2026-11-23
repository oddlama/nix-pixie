//! Hooks: programs from the user's flake that the controller runs at points
//! of the install, ordered by `after` and `before` against the install's own
//! steps and against each other.
//!
//! The steps are nodes of the same graph, chained in their fixed order, so
//! `after = [ "host-key" ]; before = [ "build" ]` places a hook between the
//! two. Every hook comes after `approve`. Among everything that may run
//! next, hooks go first (by name), so each runs as early as its constraints
//! allow.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result, bail};
use nix_pixie_proto::ctrl::PlacedFile;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::metadata::Hook;
use crate::state::private_dir;

/// The install's own steps, in the order they happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// The operator approved the target; its inventory is known.
    Approve,
    /// The facter report is in the repository (if the machine wants one).
    Facter,
    /// The pulled host key is recorded (in pull mode).
    HostKey,
    /// The disks are chosen, and recorded in pinned mode.
    Disks,
    /// The disko script and the system are built.
    Build,
    /// disko formatted the disks; the installed system's root is mounted.
    Format,
    /// The system is in the target's store.
    Copy,
    /// `nixos-install` set the profile and installed the boot loader.
    Install,
    /// Unmounted and exported; the target reboots or powers off.
    Finish,
    /// The installed system answered over SSH, with the expected key.
    Boot,
}

impl Step {
    pub const ALL: [Self; 10] = [
        Self::Approve,
        Self::Facter,
        Self::HostKey,
        Self::Disks,
        Self::Build,
        Self::Format,
        Self::Copy,
        Self::Install,
        Self::Finish,
        Self::Boot,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Facter => "facter",
            Self::HostKey => "host-key",
            Self::Disks => "disks",
            Self::Build => "build",
            Self::Format => "format",
            Self::Copy => "copy",
            Self::Install => "install",
            Self::Finish => "finish",
            Self::Boot => "boot",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which hooks run right after each step, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    after: BTreeMap<Step, Vec<String>>,
}

impl Schedule {
    /// The hooks to run once `step` is done.
    pub fn after(&self, step: Step) -> &[String] {
        self.after.get(&step).map_or(&[], Vec::as_slice)
    }

    /// Every hook with the step it follows, in running order.
    pub fn all(&self) -> impl Iterator<Item = (Step, &str)> {
        self.after
            .iter()
            .flat_map(|(step, hooks)| hooks.iter().map(move |h| (*step, h.as_str())))
    }
}

/// A node of the ordering graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    // Hooks sort before steps: they go first when both may run.
    Hook(String),
    Step(Step),
}

/// Orders `hooks` against the steps and each other. Refuses unknown names,
/// hooks named like steps, hooks before `approve`, and cycles.
pub fn schedule(hooks: &BTreeMap<String, Hook>) -> Result<Schedule> {
    let resolve = |hook: &str, name: &str| -> Result<Node> {
        if let Some(step) = Step::from_name(name) {
            return Ok(Node::Step(step));
        }
        if hooks.contains_key(name) {
            return Ok(Node::Hook(name.to_string()));
        }
        let steps: Vec<&str> = Step::ALL.iter().map(|s| s.name()).collect();
        bail!(
            "hook {hook}: `{name}` is neither a step ({}) nor a hook",
            steps.join(", ")
        )
    };

    let mut edges: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    let mut edge = |from: Node, to: Node| {
        edges.entry(from).or_default().insert(to);
    };
    for pair in Step::ALL.windows(2) {
        if let [a, b] = pair {
            edge(Node::Step(*a), Node::Step(*b));
        }
    }
    for (name, hook) in hooks {
        if Step::from_name(name).is_some() {
            bail!("hook {name} is named like a step of the install; rename it");
        }
        let me = Node::Hook(name.clone());
        edge(Node::Step(Step::Approve), me.clone());
        for after in &hook.after {
            edge(resolve(name, after)?, me.clone());
        }
        for before in &hook.before {
            let target = resolve(name, before)?;
            if target == Node::Step(Step::Approve) {
                bail!("hook {name} can't run before `approve`: hooks need an approved target");
            }
            edge(me.clone(), target);
        }
    }

    // Kahn's algorithm; the smallest ready node goes next (hooks first).
    let nodes: BTreeSet<Node> = Step::ALL
        .iter()
        .map(|s| Node::Step(*s))
        .chain(hooks.keys().map(|h| Node::Hook(h.clone())))
        .collect();
    let mut incoming: BTreeMap<&Node, usize> = nodes.iter().map(|n| (n, 0)).collect();
    for targets in edges.values() {
        for target in targets {
            if let Some(count) = incoming.get_mut(target) {
                *count += 1;
            }
        }
    }
    let mut ready: BTreeSet<&Node> = incoming
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| *node)
        .collect();
    let mut schedule = Schedule::default();
    let mut last_step = None;
    let mut done = 0;
    while let Some(node) = ready.pop_first() {
        done += 1;
        match node {
            Node::Step(step) => last_step = Some(*step),
            Node::Hook(hook) => {
                // Every hook follows `approve`, so a step was done before it.
                let step = last_step.unwrap_or(Step::Approve);
                schedule.after.entry(step).or_default().push(hook.clone());
            }
        }
        for target in edges.get(node).into_iter().flatten() {
            if let Some(count) = incoming.get_mut(target) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(target);
                }
            }
        }
    }
    if done < nodes.len() {
        let stuck: Vec<String> = incoming
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(node, _)| match node {
                Node::Hook(h) => format!("hook {h}"),
                Node::Step(s) => format!("step {s}"),
            })
            .collect();
        bail!(
            "the hooks' `after` and `before` form a cycle, involving {}",
            stuck.join(", ")
        );
    }
    Ok(schedule)
}

/// Most a hook may put into the installed system, in total. Larger things
/// belong in the configuration.
const MAX_FILES_BYTES: usize = 15 * 1024 * 1024;

/// The installable for a hook's command: a name in `flake`, or an
/// installable of its own if it has a `#`.
pub fn installable(flake: &str, command: &str) -> String {
    if command.contains('#') {
        command.to_string()
    } else {
        format!("{flake}#{command}")
    }
}

/// How a package's program is found, like `nix run` and `lib.getExe` do.
const PROGRAM: &str = "p: {
  drv = p.drvPath;
  out = p.outPath;
  program = p.meta.mainProgram or (builtins.parseDrvName p.name).name;
}";

/// Resolves each hook to its program and builds them, so a broken hook
/// fails before anything happens. Done once: running a hook later costs no
/// evaluation, and a hook that changes the repository can't change what
/// the others run.
pub async fn resolve(
    flake: &str,
    hooks: &BTreeMap<String, Hook>,
) -> Result<BTreeMap<String, PathBuf>> {
    #[derive(Deserialize)]
    struct Package {
        drv: String,
        out: String,
        program: String,
    }
    let mut packages = BTreeMap::new();
    for (name, hook) in hooks {
        let installable = installable(flake, &hook.command);
        let output = tokio::process::Command::new("nix")
            .args(["eval", "--json", &installable, "--apply", PROGRAM])
            .output()
            .await
            .context("running `nix eval` for a hook")?;
        if !output.status.success() {
            bail!(
                "hook {name}: {installable} must be a package with a main program:\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let package: Package = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("hook {name}: unexpected evaluation result"))?;
        packages.insert(name.clone(), package);
    }
    if packages.is_empty() {
        return Ok(BTreeMap::new());
    }
    let status = tokio::process::Command::new("nix")
        .args(["build", "--no-link"])
        .args(packages.values().map(|p| format!("{}^out", p.drv)))
        .stdout(Stdio::null())
        .status()
        .await
        .context("running `nix build` for the hooks")?;
    if !status.success() {
        bail!("building the hooks failed (see above)");
    }
    packages
        .into_iter()
        .map(|(name, package)| {
            let program = Path::new(&package.out).join("bin").join(&package.program);
            if !program.is_file() {
                bail!("hook {name}: {} doesn't exist", program.display());
            }
            Ok((name, program))
        })
        .collect()
}

/// What a hook learns about the install so far, in `$PIXIE_CONTEXT`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub machine: String,
    pub configuration: String,
    pub flake: String,
    /// `{ id, label, inventory }`.
    pub target: serde_json::Value,
    /// The pulled public host key (pull mode, from `host-key` on).
    pub host_key: Option<String>,
    /// The disk plan (from `disks` on).
    pub disks: Option<serde_json::Value>,
    /// The system's store path (from `build` on).
    pub system: Option<String>,
    /// Where the installed system is mounted on the target (`format` to
    /// `install`).
    pub root: Option<String>,
}

/// Runs a machine's hooks as the install passes its steps.
pub struct Runner {
    hooks: BTreeMap<String, Hook>,
    /// Each hook's program, from [`resolve`].
    programs: BTreeMap<String, PathBuf>,
    schedule: Schedule,
    /// Where hooks run: the repository.
    dir: PathBuf,
    pub context: Context,
    /// Files waiting for the installed system's root to be mounted.
    pending: Vec<PlacedFile>,
    /// Keys for disko, by path on the target, until the disks are formatted.
    disk_keys: BTreeMap<String, Zeroizing<Vec<u8>>>,
    /// Push mode: the host's private key, until the `host-key` step.
    host_key: Option<Zeroizing<Vec<u8>>>,
    /// The last step whose hooks ran.
    reached: Option<Step>,
}

impl Runner {
    pub fn new(
        hooks: BTreeMap<String, Hook>,
        programs: BTreeMap<String, PathBuf>,
        dir: PathBuf,
        context: Context,
    ) -> Result<Self> {
        Ok(Self {
            schedule: schedule(&hooks)?,
            hooks,
            programs,
            dir,
            context,
            pending: Vec::new(),
            disk_keys: BTreeMap::new(),
            host_key: None,
            reached: None,
        })
    }

    /// Runs the hooks that follow `step`. Files they write wait in
    /// [`Self::take_files`].
    pub async fn after(&mut self, step: Step) -> Result<()> {
        self.reached = Some(step);
        for name in self.schedule.after(step).to_vec() {
            let (Some(hook), Some(program)) = (self.hooks.get(&name), self.programs.get(&name))
            else {
                bail!("hook {name} was not resolved");
            };
            eprintln!("Running hook {name} (after {step})");
            let outputs = run(&name, program, hook, step, &self.dir, &self.context).await?;
            self.take_outputs(&name, step, outputs)?;
            let total: usize = self.pending.iter().map(|f| f.contents.len()).sum();
            if total > MAX_FILES_BYTES {
                bail!(
                    "the hooks' files add up to {total} bytes; at most {MAX_FILES_BYTES} can go \
                     into the installed system this way"
                );
            }
        }
        Ok(())
    }

    /// Keeps what a hook produced, if it's not too late for it.
    fn take_outputs(&mut self, name: &str, step: Step, outputs: Outputs) -> Result<()> {
        if let Some(key) = outputs.host_key {
            if step >= Step::HostKey {
                bail!(
                    "hook {name} wrote a host key after `{step}`, too late for the `host-key` \
                     step; order it before `host-key`"
                );
            }
            if self.host_key.replace(key).is_some() {
                bail!("hook {name} wrote a host key, but another hook already did");
            }
        }
        if !outputs.disk_keys.is_empty() && step > Step::Disks {
            bail!(
                "hook {name} wrote disk keys after `{step}`, but they are gathered once the \
                 disks are chosen; order it before `build`"
            );
        }
        for (path, key) in outputs.disk_keys {
            if self.disk_keys.insert(path.clone(), key).is_some() {
                bail!("hook {name} wrote a disk key for {path}, but another hook already did");
            }
        }
        self.pending.extend(outputs.files);
        Ok(())
    }

    /// Push mode: the host key a hook wrote, once.
    pub fn take_host_key(&mut self) -> Option<Zeroizing<Vec<u8>>> {
        self.host_key.take()
    }

    /// The disk keys hooks wrote, by path on the target.
    pub fn take_disk_keys(&mut self) -> BTreeMap<String, Zeroizing<Vec<u8>>> {
        std::mem::take(&mut self.disk_keys)
    }

    /// The files hooks wrote that aren't in the installed system yet.
    pub fn take_files(&mut self) -> Vec<PlacedFile> {
        std::mem::take(&mut self.pending)
    }

    /// Every hook, the step it follows, and whether it ran.
    pub fn report(&self) -> serde_json::Value {
        self.schedule
            .all()
            .map(|(step, name)| {
                serde_json::json!({
                    "name": name,
                    "after": step.name(),
                    "ran": self.reached.is_some_and(|reached| step <= reached),
                })
            })
            .collect()
    }
}

/// What a hook wrote into `$PIXIE_OUT`.
#[derive(Debug, Default)]
pub struct Outputs {
    /// `files/<path>`: for the installed system.
    pub files: Vec<PlacedFile>,
    /// `disk-keys/<path>`: for disko, at `<path>` on the target.
    pub disk_keys: BTreeMap<String, Zeroizing<Vec<u8>>>,
    /// `host-key`: the host's private key (push mode).
    pub host_key: Option<Zeroizing<Vec<u8>>>,
}

/// Reads a hook's output directory. Anything but `files/`, `disk-keys/`
/// and `host-key` is a mistake and refused.
pub fn collect_outputs(out: &Path) -> Result<Outputs> {
    let mut outputs = Outputs::default();
    for entry in std::fs::read_dir(out).with_context(|| format!("reading {}", out.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        match name.to_str() {
            Some("files") => outputs.files = collect_files(&entry.path())?,
            Some("disk-keys") => {
                outputs.disk_keys = collect_files(&entry.path())?
                    .into_iter()
                    .map(|f| (f.path, f.contents))
                    .collect();
            }
            Some("host-key") if entry.file_type()?.is_file() => {
                outputs.host_key = Some(Zeroizing::new(std::fs::read(entry.path())?));
            }
            _ => bail!(
                "{}: a hook's outputs are files/, disk-keys/ and host-key",
                name.to_string_lossy()
            ),
        }
    }
    Ok(outputs)
}

/// Runs one hook's program, in `dir`, on the operator's terminal (its stdout
/// goes to our stderr: our stdout is the result). Returns what it wrote.
async fn run(
    name: &str,
    program: &Path,
    hook: &Hook,
    step: Step,
    dir: &Path,
    context: &Context,
) -> Result<Outputs> {
    let scratch = private_dir()?;
    let result = async {
        let mut json = serde_json::to_value(context)?;
        if let Some(object) = json.as_object_mut() {
            object.insert("hook".into(), name.into());
            object.insert("step".into(), step.name().into());
        }
        let context_file = scratch.join("context.json");
        std::fs::write(&context_file, serde_json::to_vec_pretty(&json)?)
            .context("writing the hook's context")?;
        let out = scratch.join("out");
        std::fs::create_dir(&out).context("creating the hook's output directory")?;

        let status = tokio::process::Command::new(program)
            .args(&hook.args)
            .current_dir(dir)
            .env("PIXIE_CONTEXT", &context_file)
            .env("PIXIE_OUT", &out)
            .env("PIXIE_HOOK", name)
            .stdout(Stdio::from(std::io::stderr()))
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| format!("running hook {name}"))?;
        if !status.success() {
            bail!("hook {name} failed ({status})");
        }
        collect_outputs(&out).with_context(|| format!("what hook {name} wrote"))
    }
    .await;
    // Its outputs may be secrets.
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The regular files under `root`, as files for the installed system (their
/// path relative to `root` becomes the absolute path there).
pub fn collect_files(root: &Path) -> Result<Vec<PlacedFile>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is outside the outputs", path.display()))?;
            if meta.is_dir() {
                dirs.push(path);
            } else if meta.is_file() {
                let Some(relative) = relative.to_str() else {
                    bail!("{} is not UTF-8", relative.display());
                };
                files.push(PlacedFile {
                    path: format!("/{relative}"),
                    mode: meta.permissions().mode() & 0o777,
                    contents: Zeroizing::new(std::fs::read(&path)?),
                });
            } else {
                bail!(
                    "{}: only regular files can go into the installed system",
                    relative.display()
                );
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}
