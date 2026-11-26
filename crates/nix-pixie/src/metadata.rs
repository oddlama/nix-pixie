//! The user's flake as the controller sees it: the `pixie` output (see
//! `nix/lib.nix`), and where to write files back into the repository.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix_pixie_proto::inventory::{Firmware, Inventory};
use serde::Deserialize;
use tracing::{debug, info};

/// Must match `metadataVersion` in `nix/lib.nix`.
const METADATA_VERSION: u32 = 1;

/// The flake's `pixie` output (`nix/lib.nix`'s `mkMetadata`).
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub version: u32,
    pub machines: BTreeMap<String, Machine>,
}

/// One machine of the flake: which configuration it installs, and how to
/// recognise it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub configuration: String,
    #[serde(rename = "match")]
    pub hints: Hints,
    pub disks: Disks,
    pub host_key: HostKey,
    pub facter: Facter,
    #[serde(default)]
    pub firmware: FirmwareAssumed,
    #[serde(default)]
    pub secrets: Secrets,
    /// Enabled hooks, by name (see `crate::hooks`).
    pub hooks: BTreeMap<String, Hook>,
}

/// A program the controller runs at some point of the install.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hook {
    /// An app or package of the flake, or an installable with `#`.
    pub command: String,
    pub args: Vec<String>,
    /// Steps or hooks that must be done first.
    pub after: Vec<String>,
    /// Steps or hooks that must wait for it.
    pub before: Vec<String>,
}

/// Identifiers that suggest a target is this machine. Not authentication:
/// the operator approves every target.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hints {
    pub mac: Vec<String>,
    pub dmi_serial: Vec<String>,
    /// Serial numbers from DMI or the device tree.
    #[serde(default)]
    pub serial: Vec<String>,
}

/// Where the installed system's SSH host key comes from.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKey {
    pub mode: HostKeyMode,
    /// Pull mode: where to record the public key, relative to the flake.
    pub public_key_path: Option<String>,
}

/// See `hostKey.mode` in `nix/options.nix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKeyMode {
    /// The installed system makes its own on first boot.
    None,
    /// The controller provides it (not implemented yet).
    Push,
    /// The target generates it during the install.
    Pull,
}

/// How the machine's disks are chosen.
#[derive(Debug, Clone, Deserialize)]
pub struct Disks {
    /// Pinned mode: where the chosen disks are recorded, relative to the
    /// flake. `None`: whatever the disko config names.
    pub file: Option<String>,
    /// Which disk a role takes when nobody says, by role.
    #[serde(default)]
    pub rules: BTreeMap<String, crate::plan::DiskRule>,
}

/// The placeholder in per-device paths for the install's id.
pub const ID: &str = "{id}";

/// Checks an install's id (`--id`, or a hook's `$PIXIE_OUT/id`): it goes
/// into paths in the repository.
///
/// # Errors
///
/// If it is anything but 1 to 64 letters, digits, `.`, `_` and `-`, or
/// starts with a `.`.
pub fn check_id(id: &str) -> Result<()> {
    let plain = (1..=64).contains(&id.len())
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !plain {
        bail!(
            "{id:?} is not an id: 1 to 64 letters, digits, `.`, `_` and `-`, not starting with `.`"
        );
    }
    Ok(())
}

/// `path` (the machine's `option`) with `{id}` replaced by the install's
/// id.
///
/// # Errors
///
/// If the path uses `{id}` and there is no id.
pub fn with_id(path: &str, id: Option<&str>, option: &str) -> Result<String> {
    if !path.contains(ID) {
        return Ok(path.to_string());
    }
    let id = id.with_context(|| {
        format!(
            "{option} ({path}) uses {ID}, but this install has no id: pass --id, or have a hook \
             write it to $PIXIE_OUT/id before it is needed"
        )
    })?;
    Ok(path.replace(ID, id))
}

/// Where secrets hooks give the controller for the repository go.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Secrets {
    /// Age recipients or SSH public keys they are encrypted to.
    #[serde(default)]
    pub recipients: Vec<String>,
}

/// What to assume about the machine's firmware, where the installer can't
/// tell; `None`: as the installer sees it.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareAssumed {
    pub uefi: Option<bool>,
    pub setup_mode: Option<bool>,
}

impl FirmwareAssumed {
    /// The firmware the installer saw, with what is assumed instead.
    pub fn apply(self, seen: &Firmware) -> Firmware {
        Firmware {
            uefi: self.uefi.unwrap_or(seen.uefi),
            secure_boot: seen.secure_boot,
            setup_mode: self.setup_mode.or(seen.setup_mode),
        }
    }
}

/// Where the machine's nixos-facter report goes, if it wants one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facter {
    pub report_path: Option<String>,
    #[serde(default)]
    pub mode: crate::facter::Mode,
    /// Fields to leave out when comparing, besides [`crate::facter::VOLATILE`].
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// A machine whose hints match a target, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub machine: String,
    pub reasons: Vec<String>,
}

/// Evaluates `<flake>#pixie` with the user's `nix`.
pub async fn load(flake: &str) -> Result<Metadata> {
    let started = std::time::Instant::now();
    let attr = format!("{flake}#pixie");
    let output = crate::nix::command()
        .args(["eval", "--json", &attr])
        .output()
        .await
        .context("running `nix eval` (is Nix installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not provide attribute") {
            bail!(
                "{flake} has no `pixie` output; add nix-pixie's flakeModules.default or lib.mkPixie"
            );
        }
        bail!("`nix eval {attr}` failed:\n{}", stderr.trim());
    }
    let metadata = parse(&output.stdout)?;
    info!(
        %flake,
        machines = metadata.machines.len(),
        seconds = started.elapsed().as_secs_f64(),
        "loaded the machines"
    );
    Ok(metadata)
}

pub fn parse(json: &[u8]) -> Result<Metadata> {
    let metadata: Metadata =
        serde_json::from_slice(json).context("unexpected format of the `pixie` output")?;
    if metadata.version != METADATA_VERSION {
        bail!(
            "the flake's `pixie` output has version {}, this nix-pixie understands {METADATA_VERSION}; \
             use matching versions of the nix-pixie flake and binary",
            metadata.version
        );
    }
    Ok(metadata)
}

impl Metadata {
    /// Machines whose hints match the target. Hints identify, they don't
    /// authenticate: the operator still approves.
    pub fn suggest(&self, inventory: &Inventory) -> Vec<Suggestion> {
        self.machines
            .iter()
            .filter_map(|(name, machine)| {
                let mut reasons: Vec<String> = inventory
                    .nics
                    .iter()
                    .filter(|nic| {
                        machine
                            .hints
                            .mac
                            .iter()
                            .any(|mac| mac.eq_ignore_ascii_case(&nic.mac))
                    })
                    .map(|nic| format!("MAC {} ({})", nic.mac, nic.name))
                    .collect();
                if let Some(serial) = &inventory.dmi.serial
                    && (machine.hints.dmi_serial.contains(serial)
                        || machine.hints.serial.contains(serial))
                {
                    reasons.push(format!("DMI serial {serial}"));
                }
                if let Some(serial) = &inventory.device_tree.serial
                    && machine.hints.serial.contains(serial)
                {
                    reasons.push(format!("device-tree serial {serial}"));
                }
                (!reasons.is_empty()).then(|| Suggestion {
                    machine: name.clone(),
                    reasons,
                })
            })
            .collect()
    }
}

/// The directory of a flake reference that points at a local checkout, if
/// it does. Files can only be written back into those.
pub fn local_flake_dir(flake: &str) -> Option<PathBuf> {
    let path = flake
        .strip_prefix("path:")
        .or_else(|| flake.strip_prefix("git+file://"))
        .map_or(flake, |rest| rest.split(['?', '#']).next().unwrap_or(rest));
    // Anything else with a scheme (github:, git+https:, ...) is remote.
    if path.contains(':') {
        return None;
    }
    let dir = std::fs::canonicalize(path).ok()?;
    dir.join("flake.nix").is_file().then_some(dir)
}

/// `relative` inside `root`, refusing anything that could escape it. The
/// path comes from the user's flake, so this is a second line of defence
/// behind the option type's check.
pub fn repo_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    if relative.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("`{relative}` is not a plain path inside the repository");
    }
    Ok(root.join(rel))
}

/// Writes `contents` to `path` (creating directories) and `git add`s it, since
/// flakes ignore untracked files. Never commits.
pub async fn write_into_repo(root: &Path, path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("nix-pixie-tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;

    let in_git = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .is_ok_and(|o| o.status.success());
    if in_git {
        let added = tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("add")
            .arg("--")
            .arg(path)
            .output()
            .await
            .context("running `git add`")?;
        anyhow::ensure!(
            added.status.success(),
            "`git add {}` failed: {}",
            path.display(),
            String::from_utf8_lossy(&added.stderr).trim()
        );
        debug!(path = %path.display(), "git add");
    }
    Ok(())
}
