//! The target's side of formatting: checking the disks the operator chose,
//! linking the ones disko addresses by role, and running the disko script
//! the controller built and pushed into the live store.
//!
//! The controller decides what to format; the agent re-checks right before
//! anything happens, because the inventory the operator saw may be stale (a
//! USB stick plugged in since) and because a buggy or confused controller
//! must still not wipe the installer's own boot medium.

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use nix_pixie_proto::ctrl::{DiskAssignment, TargetMsg};
use nix_pixie_proto::frame::write_frame;
use nix_pixie_proto::inventory::Disk;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::{info, trace};

use crate::inventory;
use crate::net::Stream;
use crate::state::StateDir;
use crate::store::is_store_path;

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
pub async fn prepare(disks: &[DiskAssignment], formatted: &BTreeSet<String>) -> Result<(), String> {
    check_assignments(disks)?;
    let present = inventory::disks(formatted)
        .await
        .map_err(|e| format!("can't list the disks: {e:#}"))?;
    check_present(disks, &present)?;
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
        if !disk.installable() {
            return Err(format!(
                "{} is in use (mounted or swap) or read-only; it may be the installer's own disk",
                wanted.device
            ));
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

    info!(%script, "running disko");
    let mut child = match tokio::process::Command::new(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Ok(Err(format!("starting {script}: {e}"))),
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Ok(Err("disko's output is not connected".into()));
    };
    forward(stream, stdout, stderr).await?;
    let status = child.wait().await.context("waiting for disko")?;
    info!(%status, "disko finished");
    Ok(if status.success() {
        Ok(())
    } else {
        Err(format!("disko failed ({status})"))
    })
}

/// Sends both pipes' data as `DiskoOutput` frames until both are closed.
async fn forward(
    stream: &mut Stream,
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
) -> Result<()> {
    let mut out_buf = vec![0; 16 * 1024];
    let mut err_buf = vec![0; 16 * 1024];
    let (mut out_open, mut err_open) = (true, true);
    loop {
        // `read` is cancel-safe: a branch that loses the race loses no data.
        // A read error counts as the end of that pipe.
        let (n, from_stdout) = tokio::select! {
            r = stdout.read(&mut out_buf), if out_open => (r.unwrap_or(0), true),
            r = stderr.read(&mut err_buf), if err_open => (r.unwrap_or(0), false),
            else => return Ok(()),
        };
        if n == 0 {
            if from_stdout {
                out_open = false;
            } else {
                err_open = false;
            }
            continue;
        }
        trace!(bytes = n, "disko output");
        let buf = if from_stdout { &out_buf } else { &err_buf };
        let chunk = buf.get(..n).unwrap_or_default().to_vec();
        write_frame(stream, &TargetMsg::DiskoOutput { chunk }).await?;
    }
}
