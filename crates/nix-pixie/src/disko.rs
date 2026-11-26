//! The target's side of formatting: checking the disks the operator chose,
//! linking the ones disko addresses by role, and running the disko script
//! the controller built and pushed into the live store.
//!
//! The controller decides what to format; the agent re-checks right before
//! anything happens, because the inventory the operator saw may be stale (a
//! USB stick plugged in since) and because a buggy or confused controller
//! must still not wipe the installer's own boot medium.

use std::collections::BTreeSet;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};

use crate::child;
use crate::plan;
use nix_pixie_proto::ctrl::{DiskAssignment, KeyFile};
use nix_pixie_proto::inventory::Disk;
use tracing::{info, warn};

use crate::inventory;
use crate::net::Stream;
use crate::state::StateDir;
use crate::store::{self, is_store_path};

/// Where agnostic disko configs find their disks.
pub const BY_PIXIE: &str = "/dev/disk/by-pixie";
/// State file listing the disks formatted during this boot.
const FORMATTED: &str = "formatted";

/// Disks nix-pixie formatted during this boot (kernel names). Kept in the
/// state directory, which is on tmpfs, so it lasts exactly one boot and
/// survives agent restarts.
pub fn formatted(state: &StateDir) -> BTreeSet<String> {
    state.read(FORMATTED).map_or_else(BTreeSet::new, |text| {
        text.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn remember_formatted(state: &StateDir, disks: &[DiskAssignment]) -> Result<()> {
    let mut all = formatted(state);
    all.extend(disks.iter().map(|d| d.device.clone()));
    state.write(FORMATTED, &all.into_iter().collect::<Vec<_>>().join("\n"))
}

/// A role or kernel name we are willing to put into a path.
pub fn is_plain_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Checks `disks` against a fresh look at the machine and links the agnostic
/// ones. `Err` is a reason for the operator.
pub async fn prepare(
    disks: &[DiskAssignment],
    labels: &[String],
    formatted: &BTreeSet<String>,
) -> Result<(), String> {
    check_assignments(disks)?;
    let present = inventory::disks(formatted)
        .await
        .map_err(|e| format!("can't list the disks: {e:#}"))?;
    check_present(disks, &present)?;
    let formatting = disks.iter().map(|d| d.device.as_str()).collect();
    plan::check_name_clashes(&labels.iter().cloned().collect(), &formatting, &present)?;
    for disk in disks.iter().filter(|d| d.link) {
        link(Path::new(BY_PIXIE), &disk.role, &disk.device)
            .map_err(|e| format!("linking {BY_PIXIE}/{}: {e:#}", disk.role))?;
    }
    Ok(())
}

/// Every disk must be `present` and installable right now.
pub fn check_present(disks: &[DiskAssignment], present: &[Disk]) -> Result<(), String> {
    for wanted in disks {
        let disk = present
            .iter()
            .find(|d| d.name == wanted.device)
            .ok_or_else(|| format!("{} is gone", wanted.device))?;
        if let Some(reason) = plan::unusable(disk) {
            return Err(reason);
        }
    }
    Ok(())
}

/// Names must be plain, and no role or disk may appear twice.
pub fn check_assignments(disks: &[DiskAssignment]) -> Result<(), String> {
    if disks.is_empty() {
        return Err("no disks to format".into());
    }
    let mut roles = BTreeSet::new();
    let mut devices = BTreeSet::new();
    for disk in disks {
        if !is_plain_name(&disk.role) || !is_plain_name(&disk.device) {
            return Err("invalid disk or role name".into());
        }
        if !roles.insert(&disk.role) {
            return Err(format!("role {} assigned twice", disk.role));
        }
        if !devices.insert(&disk.device) {
            return Err(format!("{} assigned to two roles", disk.device));
        }
    }
    Ok(())
}

/// Points `<dir>/<role>` at the disk, relative like udev's own links, and
/// replaces whatever was there atomically.
pub fn link(dir: &Path, role: &str, device: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(role);
    let tmp = dir.join(format!(".{role}.nix-pixie-tmp"));
    let _ = std::fs::remove_file(&tmp);
    symlink(Path::new("../..").join(device), &tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Runs the disko `script` for the prepared `disks`, sending its output to the
/// controller as it comes. The outer `Result` is about the control stream;
/// the inner one is disko's outcome, as a reason for the operator.
pub async fn run(
    state: &StateDir,
    stream: &mut Stream,
    script: &str,
    disks: &[DiskAssignment],
    key_files: &[KeyFile],
) -> Result<Result<(), String>> {
    if !is_store_path(script) {
        return Ok(Err("not a store path".into()));
    }
    let valid = tokio::process::Command::new("nix-store")
        .args(["--check-validity", script])
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !valid {
        return Ok(Err(format!("{script} is not in the live store")));
    }
    // Before anything happens: once disko has started, these disks hold
    // nothing but this attempt, even if it fails halfway.
    if let Err(e) = remember_formatted(state, disks) {
        return Ok(Err(format!("{e:#}")));
    }

    let written = match write_key_files(key_files) {
        Ok(written) => written,
        Err(reason) => return Ok(Err(reason)),
    };
    info!(%script, key_files = written.len(), "running disko");
    let result = child::run("disko", tokio::process::Command::new(script), stream).await;
    // Whatever happened: the keys are of no use afterwards.
    for path in &written {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(path = %path.display(), "removing a key file: {e}");
        }
    }
    result
}

/// Directories key files may go into. The installer's `/tmp` and `/run`
/// are in RAM; `check_key_path` makes sure.
const KEY_DIRS: [&str; 2] = ["/tmp", "/run"];

/// Where a key file may be written: a plain path under `/tmp` or `/run`,
/// on a filesystem in RAM according to `mountinfo`, so the key never
/// touches a disk. `Err` is a reason for the operator.
pub fn check_key_path(path: &str, mountinfo: &str) -> Result<(), String> {
    let candidate = Path::new(path);
    let plain = candidate
        .components()
        .skip(1)
        .all(|c| matches!(c, Component::Normal(_)));
    let under = KEY_DIRS
        .iter()
        .any(|dir| candidate.starts_with(dir) && candidate != Path::new(dir));
    if !candidate.is_absolute() || !plain || !under {
        return Err(format!(
            "{path}: key files must be plain paths under {}",
            KEY_DIRS.join(" or ")
        ));
    }
    let fstype = store::mounts(mountinfo)
        .filter(|(point, _)| candidate.starts_with(point))
        .max_by_key(|(point, _)| point.components().count())
        .map(|(_, fstype)| fstype);
    match fstype.as_deref() {
        // `rootfs`: an initramfs's root, which is in RAM too (an installer
        // that is only an initrd keeps /tmp there).
        Some("tmpfs" | "ramfs" | "rootfs") => Ok(()),
        Some(other) => Err(format!(
            "{path} would be on {other}, and keys must never touch a disk"
        )),
        None => Err(format!("{path}: can't tell which filesystem it is on")),
    }
}

/// Writes the key files (0400, in 0700 directories it creates) and returns
/// their paths. On failure, removes the ones already written.
fn write_key_files(files: &[KeyFile]) -> Result<Vec<PathBuf>, String> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading /proc/self/mountinfo: {e}"))?;
    let mut written = Vec::new();
    for file in files {
        let result = check_key_path(&file.path, &mountinfo).and_then(|()| {
            write_key_file(Path::new(&file.path), &file.contents)
                .map_err(|e| format!("writing {}: {e:#}", file.path))
        });
        if let Err(reason) = result {
            for path in &written {
                let _ = std::fs::remove_file(path);
            }
            return Err(reason);
        }
        written.push(PathBuf::from(&file.path));
    }
    Ok(written)
}

fn write_key_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    std::io::Write::write_all(&mut file, contents)?;
    Ok(())
}
