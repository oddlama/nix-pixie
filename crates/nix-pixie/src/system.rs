//! The NixOS configuration being installed, as the controller sees it: a few
//! facts evaluated from it, and the paths built from it. Uses the operator's
//! `nix`, so builds go wherever their Nix sends them (remote builders,
//! binfmt).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::info;

/// What the controller needs to know before building: evaluated without
/// building anything. Devices that aren't set evaluate to `null` instead of
/// failing, so the controller can say what's missing.
const FACTS: &str = r#"config: {
  system = config.nixpkgs.hostPlatform.system;
  disko =
    if config ? disko then {
      rootMountPoint = config.disko.rootMountPoint;
      disks = builtins.mapAttrs (_: disk: {
        device = let d = builtins.tryEval disk.device; in if d.success then d.value else null;
        partLabels =
          if (disk.content.type or null) == "gpt"
          then map (p: p.label) (builtins.attrValues disk.content.partitions)
          else [ ];
      }) config.disko.devices.disk;
      zpools = builtins.attrNames (config.disko.devices.zpool or { });
    } else null;
  ssh = {
    inherit (config.services.openssh) enable ports;
    hostKeyPath =
      let keys = builtins.filter (k: (k.type or null) == "ed25519") config.services.openssh.hostKeys;
      in if keys == [ ] then null else (builtins.head keys).path;
  };
}"#;

/// `nixosConfigurations.<name>` of a flake.
#[derive(Debug, Clone)]
pub struct Configuration {
    flake: String,
    name: String,
}

/// Facts about a configuration, see [`FACTS`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Facts {
    /// `nixpkgs.hostPlatform.system`, e.g. `x86_64-linux`.
    pub system: String,
    /// `None` when the configuration doesn't import disko.
    pub disko: Option<Disko>,
    pub ssh: Ssh,
}

/// The configuration's SSH server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ssh {
    /// `services.openssh.enable`: whether the host key can be checked after
    /// the first boot.
    pub enable: bool,
    pub ports: Vec<u16>,
    /// The first ed25519 entry of `services.openssh.hostKeys`: where a
    /// pulled host key goes.
    pub host_key_path: Option<String>,
}

/// The configuration's disko setup.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disko {
    /// Where the disko script mounts everything, `/mnt` by default.
    pub root_mount_point: String,
    /// `disko.devices.disk`, by name.
    pub disks: BTreeMap<String, DiskoDisk>,
    /// `disko.devices.zpool` names, exported before the first boot.
    pub zpools: Vec<String>,
}

/// One of disko's disks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskoDisk {
    /// The device path, `None` if the configuration doesn't set it.
    pub device: Option<String>,
    /// GPT partition labels disko gives it. disko mounts by these, so no
    /// other disk on the target may carry them.
    pub part_labels: Vec<String>,
}

/// What gets pushed to the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// `system.build.diskoScript`: destroy, format and mount.
    pub disko_script: String,
    /// `system.build.toplevel`.
    pub toplevel: String,
}

impl Configuration {
    /// `name` must be a plain attribute name; it ends up in an attribute path.
    pub fn new(flake: &str, name: &str) -> Result<Self> {
        let plain = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'\''));
        if !plain {
            bail!("nixosConfigurations.{name:?}: nix-pixie only handles plain attribute names");
        }
        Ok(Self {
            flake: flake.to_string(),
            name: name.to_string(),
        })
    }

    /// The configuration's attribute name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The installable for `config<path>`, e.g. `.system.build.toplevel`.
    fn attr(&self, path: &str) -> String {
        format!(
            "{}#nixosConfigurations.\"{}\".config{path}",
            self.flake, self.name
        )
    }

    /// Evaluates [`Facts`]. Cheap compared to building.
    pub async fn facts(&self) -> Result<Facts> {
        let attr = self.attr("");
        let output = tokio::process::Command::new("nix")
            .args(["eval", "--json", &attr, "--apply", FACTS])
            .output()
            .await
            .context("running `nix eval` (is Nix installed?)")?;
        if !output.status.success() {
            bail!(
                "evaluating nixosConfigurations.{} failed:\n{}",
                self.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout)
            .with_context(|| format!("unexpected facts about nixosConfigurations.{}", self.name))
    }

    /// Builds the disko script and the system in one evaluation. Nix shows
    /// its progress on our stderr.
    pub async fn build(&self) -> Result<Built> {
        let started = std::time::Instant::now();
        let output = tokio::process::Command::new("nix")
            .args(["build", "--no-link", "--json"])
            .arg(self.attr(".system.build.diskoScript"))
            .arg(self.attr(".system.build.toplevel"))
            .stderr(std::process::Stdio::inherit())
            .output()
            .await
            .context("running `nix build`")?;
        if !output.status.success() {
            bail!(
                "building nixosConfigurations.{} failed (see above)",
                self.name
            );
        }
        let built = parse_build_output(&output.stdout)?;
        info!(
            seconds = started.elapsed().as_secs_f64(),
            toplevel = %built.toplevel,
            "built"
        );
        Ok(built)
    }
}

/// `nix build --json` lists results in the order of the installables.
pub fn parse_build_output(json: &[u8]) -> Result<Built> {
    #[derive(Deserialize)]
    struct Result {
        outputs: BTreeMap<String, String>,
    }
    let results: Vec<Result> =
        serde_json::from_slice(json).context("unexpected output of `nix build --json`")?;
    let out = |r: &Result| r.outputs.get("out").cloned();
    match results.as_slice() {
        [disko, toplevel] => Ok(Built {
            disko_script: out(disko).context("the disko script has no `out`")?,
            toplevel: out(toplevel).context("the system has no `out`")?,
        }),
        other => bail!("`nix build` reported {} results, expected 2", other.len()),
    }
}
