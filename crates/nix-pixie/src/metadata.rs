//! The user's flake as the controller sees it: the `pixie` output (see
//! `nix/lib.nix`), and where to write files back into the repository.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix_pixie_proto::inventory::Inventory;
use serde::Deserialize;

/// Must match `metadataVersion` in `nix/lib.nix`.
const METADATA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub version: u32,
    pub machines: BTreeMap<String, Machine>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Machine {
    pub configuration: String,
    #[serde(rename = "match")]
    pub hints: Hints,
    pub facter: Facter,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hints {
    pub mac: Vec<String>,
    pub dmi_serial: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facter {
    pub report_path: Option<String>,
}

/// A machine whose hints match a target, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub machine: String,
    pub reasons: Vec<String>,
}

/// Evaluates `<flake>#pixie` with the user's `nix`.
pub async fn load(flake: &str) -> Result<Metadata> {
    let attr = format!("{flake}#pixie");
    let output = tokio::process::Command::new("nix")
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
    parse(&output.stdout)
}

fn parse(json: &[u8]) -> Result<Metadata> {
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
                    && machine.hints.dmi_serial.contains(serial)
                {
                    reasons.push(format!("DMI serial {serial}"));
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
        .map(|rest| rest.split(['?', '#']).next().unwrap_or(rest))
        .unwrap_or(flake);
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
            .status()
            .await
            .context("running `git add`")?;
        anyhow::ensure!(added.success(), "`git add {}` failed", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix_pixie_proto::inventory::{Dmi, Nic};

    const METADATA: &str = r#"{
        "version": 1,
        "machines": {
            "web1": {
                "configuration": "web1",
                "match": { "mac": ["52:54:00:12:01:02"], "dmiSerial": [] },
                "disks": { "mode": "pinned", "roles": ["main"] },
                "hostKey": { "mode": "none" },
                "facter": { "reportPath": "machines/web1/facter.json" }
            },
            "db1": {
                "configuration": "db",
                "match": { "mac": [], "dmiSerial": ["SN-42"] },
                "disks": { "mode": "agnostic", "roles": ["main"] },
                "hostKey": { "mode": "pull" },
                "facter": { "reportPath": null }
            }
        }
    }"#;

    fn inventory(mac: &str, serial: Option<&str>) -> Inventory {
        Inventory {
            nics: vec![Nic {
                name: "eth1".into(),
                mac: mac.into(),
                link_up: true,
            }],
            dmi: Dmi {
                serial: serial.map(Into::into),
                ..Dmi::default()
            },
            ..Inventory::default()
        }
    }

    #[test]
    fn parses_and_suggests() {
        let metadata = parse(METADATA.as_bytes()).unwrap();
        assert_eq!(metadata.machines["db1"].configuration, "db");
        assert_eq!(
            metadata.machines["web1"].facter.report_path.as_deref(),
            Some("machines/web1/facter.json")
        );

        let by_mac = metadata.suggest(&inventory("52:54:00:12:01:02", None));
        assert_eq!(
            by_mac,
            [Suggestion {
                machine: "web1".into(),
                reasons: vec!["MAC 52:54:00:12:01:02 (eth1)".into()]
            }]
        );
        let by_serial = metadata.suggest(&inventory("aa:aa:aa:aa:aa:aa", Some("SN-42")));
        assert_eq!(
            by_serial
                .iter()
                .map(|s| s.machine.as_str())
                .collect::<Vec<_>>(),
            ["db1"]
        );
        assert!(
            metadata
                .suggest(&inventory("aa:aa:aa:aa:aa:aa", None))
                .is_empty()
        );
    }

    #[test]
    fn rejects_other_versions() {
        let other = METADATA.replace("\"version\": 1", "\"version\": 2");
        let err = parse(other.as_bytes()).unwrap_err().to_string();
        assert!(err.contains("version 2"), "{err}");
    }

    #[test]
    fn repo_paths_stay_inside() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_path(root, "machines/a/facter.json").unwrap(),
            Path::new("/repo/machines/a/facter.json")
        );
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "./a"] {
            assert!(repo_path(root, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn recognises_local_flakes() {
        let dir = std::env::temp_dir().join(format!("nix-pixie-flake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flake.nix"), "{}").unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let path = dir.to_str().unwrap();
        assert_eq!(local_flake_dir(path), Some(canonical.clone()));
        assert_eq!(
            local_flake_dir(&format!("path:{path}")),
            Some(canonical.clone())
        );
        assert_eq!(
            local_flake_dir(&format!("git+file://{path}?ref=main")),
            Some(canonical)
        );
        assert_eq!(local_flake_dir("github:oddlama/infra"), None);
        assert_eq!(local_flake_dir("/nonexistent/flake"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
