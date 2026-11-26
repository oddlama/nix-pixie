//! The controller's side of hardware reports: getting one from the target,
//! and comparing one with the report a machine already has, for many
//! devices of identical hardware that share it (`facter.mode = "compare"`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use nix_pixie_proto::ctrl::{ControllerMsg, TargetMsg};
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use crate::link::Link;
use crate::metadata;
use crate::net::sanitise;

/// nixos-facter takes a few seconds; give slow machines plenty of room.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// Differences listed at most.
const MAX_DIFFERENCES: usize = 50;

/// What a machine does with its report (`facter.mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Get it and write it into the repository, every install.
    #[default]
    Write,
    /// Only if the repository has none yet: identical devices share it.
    Missing,
    /// Get it, write nothing, and report how it differs from the one in
    /// the repository (written if there is none).
    Compare,
}

/// Fields that differ from device to device of identical hardware, at any
/// depth: serial numbers, MAC addresses, ids. `facter.ignore` adds more.
pub const VOLATILE: &[&str] = &[
    "serial",
    "serial_number",
    "unique_id",
    "uuid",
    "mac",
    "hw_address",
    "permanent_hw_address",
];

/// What happened to the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Written,
    /// There was one, and it wasn't asked for.
    Kept,
    /// It was compared: what differs, as JSON pointers.
    Compared(Vec<String>),
}

/// Asks the target for its report: a JSON object.
pub async fn request(link: &Link) -> Result<(Vec<u8>, Value)> {
    let reply = link
        .request(
            &ControllerMsg::RequestFacterReport,
            TIMEOUT,
            "the facter report",
        )
        .await?;
    let json = match reply {
        TargetMsg::FacterReport { json } => json,
        TargetMsg::FacterFailed { error } => {
            bail!(
                "the target could not run nixos-facter: {}",
                sanitise(&error, 500)
            )
        }
        _ => bail!("unexpected reply to the facter request"),
    };
    // It goes into the user's repository and gets imported by Nix: insist on
    // a JSON object.
    let value: Value =
        serde_json::from_slice(&json).context("the facter report is not valid JSON")?;
    anyhow::ensure!(value.is_object(), "the facter report is not a JSON object");
    Ok((json, value))
}

/// Gets the target's report and does with it what `mode` says, at
/// `report_path` in the repository of `flake`.
pub async fn fetch(
    link: &Link,
    flake: &str,
    report_path: &str,
    mode: Mode,
    ignore: &[String],
) -> Result<Outcome> {
    let root = metadata::local_flake_dir(flake).with_context(|| {
        format!("{flake} is not a local checkout, so the report can't be written")
    })?;
    let path = metadata::repo_path(&root, report_path)?;
    let existing = std::fs::read(&path).ok();
    if mode == Mode::Missing && existing.is_some() {
        info!(path = %path.display(), "kept the facter report");
        return Ok(Outcome::Kept);
    }
    let (json, value) = request(link).await?;
    if let (Mode::Compare, Some(existing)) = (mode, existing) {
        let known: Value = serde_json::from_slice(&existing)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        let differences = compare(&known, &value, ignore);
        info!(path = %path.display(), differences = differences.len(), "compared the facter report");
        return Ok(Outcome::Compared(differences));
    }
    write(&root, &path, &json).await?;
    Ok(Outcome::Written)
}

async fn write(root: &Path, path: &Path, json: &[u8]) -> Result<()> {
    metadata::write_into_repo(root, path, json).await?;
    info!(path = %path.display(), "wrote facter report");
    Ok(())
}

/// Where `new` differs from `known`, as JSON pointers, leaving out fields
/// named in [`VOLATILE`] or `ignore` at any depth. At most
/// [`MAX_DIFFERENCES`], the last saying how many more there are.
pub fn compare(known: &Value, new: &Value, ignore: &[String]) -> Vec<String> {
    let mut differences = Vec::new();
    walk(known, new, String::new(), ignore, &mut differences);
    if differences.len() > MAX_DIFFERENCES {
        let more = differences.len() - MAX_DIFFERENCES;
        differences.truncate(MAX_DIFFERENCES);
        differences.push(format!("... and {more} more"));
    }
    differences
}

fn walk(known: &Value, new: &Value, at: String, ignore: &[String], out: &mut Vec<String>) {
    let ignored = |key: &str| VOLATILE.contains(&key) || ignore.iter().any(|i| i == key);
    match (known, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys.into_iter().filter(|k| !ignored(k)) {
                let here = format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => walk(x, y, here, ignore, out),
                    _ => out.push(here),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{at} ({} entries, now {})", a.len(), b.len()));
                return;
            }
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                walk(x, y, format!("{at}/{i}"), ignore, out);
            }
        }
        (a, b) if a != b => out.push(if at.is_empty() { "/".into() } else { at }),
        _ => {}
    }
}
