//! Which of the target's disks each of the configuration's disko disks goes
//! onto. A disko disk is addressed in one of three ways:
//!
//! * **Pinned**: the machine has `disks.file`. The operator chooses a disk
//!   (or keeps the recorded one), and the controller records its
//!   `/dev/disk/by-id` path for `lib.pinnedDisk`.
//! * **By role**: the device is `/dev/disk/by-pixie/<role>`. The operator
//!   chooses, and the agent links the disk there for the install.
//! * **Fixed**: any other device. It must exist on the target.
//!
//! Decisions only, no I/O, so all of it is tested without a target.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use nix_pixie_proto::inventory::Disk;
use serde::Serialize;

use crate::disko::is_plain_name;
use crate::net::sanitise;
use crate::system::Disko;

const BY_ID: &str = "/dev/disk/by-id/";
const BY_PIXIE: &str = "/dev/disk/by-pixie/";

/// How a disko disk finds its target disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Pinned mode. `recorded`: the disk the configuration currently names,
    /// if it is on this target.
    Pinned { recorded: Option<String> },
    /// `/dev/disk/by-pixie/<role>`.
    ByRole,
    /// The configuration names this device, found on the target as `disk`.
    Fixed { device: String, disk: String },
}

/// One disko disk and how it is addressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// `disko.devices.disk.<name>`.
    pub name: String,
    /// What `--disk ROLE=DISK` calls it: the by-pixie role, else `name`.
    pub role: String,
    pub source: Source,
}

/// A disko disk with the target disk it goes onto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choice {
    /// `disko.devices.disk.<name>`.
    pub name: String,
    pub role: String,
    /// Kernel name on the target, e.g. `nvme0n1`.
    pub disk: String,
    /// Pinned mode: the device path to record.
    pub pin: Option<String>,
    /// Agnostic mode: the agent links `/dev/disk/by-pixie/<role>` to it.
    pub link: bool,
}

/// What [`resolve`] needs to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Done(Vec<Choice>),
    /// The operator has to choose a disk for `role` among `candidates`
    /// (kernel names).
    Ask {
        role: String,
        candidates: Vec<String>,
    },
}

/// The roles `--disk` may name. Needs no target, so typos fail before
/// pairing.
pub fn choosable_roles(disko: &Disko, pinned: bool) -> BTreeSet<String> {
    disko
        .disks
        .iter()
        .filter_map(|(name, disk)| {
            if pinned {
                return Some(name.clone());
            }
            disk.device
                .as_deref()
                .and_then(|d| d.strip_prefix(BY_PIXIE))
                .map(str::to_string)
        })
        .collect()
}

/// Agnostic disks need `nixosModules.default`: on the installed system no
/// agent links `/dev/disk/by-pixie/<role>`, and some options keep using it
/// (GRUB on BIOS installs to it on every rebuild). In pinned mode the
/// configuration falls back to these paths only until the disks are
/// recorded, which happens before the build.
pub fn check_links(disko: &Disko, pinned: bool, configuration: &str) -> Result<()> {
    if pinned || disko.by_pixie_links {
        return Ok(());
    }
    let Some((name, device)) = disko.disks.iter().find_map(|(name, disk)| {
        let device = disk.device.as_deref()?;
        device.starts_with(BY_PIXIE).then_some((name, device))
    }) else {
        return Ok(());
    };
    bail!(
        "disko.devices.disk.{name} uses {device}, but nixosConfigurations.{configuration} \
         doesn't import nix-pixie.nixosModules.default, which keeps that path working on the \
         installed system (GRUB on BIOS machines installs to it on every rebuild). Add the \
         module, or pin the disk instead (disks.file and nix-pixie.lib.pinnedDisk)"
    )
}

/// Checks `--disk` arguments against the configuration, before pairing.
pub fn check_requested(
    disko: &Disko,
    pinned: bool,
    requested: &BTreeMap<String, String>,
) -> Result<()> {
    let roles = choosable_roles(disko, pinned);
    for role in requested.keys() {
        if !roles.contains(role) {
            let known: Vec<&str> = roles.iter().map(String::as_str).collect();
            if known.is_empty() {
                bail!(
                    "--disk {role}=...: this configuration fixes all its devices, there is nothing to choose"
                );
            }
            bail!(
                "--disk {role}=...: no such role; the configuration has {}",
                known.join(", ")
            );
        }
    }
    Ok(())
}

/// How each disko disk is addressed, looking up fixed devices on the target.
pub fn slots(disko: &Disko, pinned: bool, present: &[Disk]) -> Result<Vec<Slot>> {
    if disko.disks.is_empty() {
        bail!("the configuration's disko.devices.disk is empty");
    }
    let mut roles = BTreeSet::new();
    let mut slots = Vec::new();
    for (name, disk) in &disko.disks {
        let device = disk.device.as_deref();
        let (role, source) = if pinned {
            let recorded = device
                .and_then(|d| d.strip_prefix(BY_ID))
                .and_then(|id| present.iter().find(|d| d.by_id.iter().any(|b| b == id)))
                .map(|d| d.name.clone());
            (name.clone(), Source::Pinned { recorded })
        } else {
            match device {
                None => bail!(
                    "disko.devices.disk.{name}.device is not set. Set it, use {BY_PIXIE}{name} \
                     (agnostic mode), or give the machine a disks.file and read it with \
                     nix-pixie.lib.pinnedDisk (pinned mode)"
                ),
                Some(device) => match device.strip_prefix(BY_PIXIE) {
                    Some(role) if is_plain_name(role) => (role.to_string(), Source::ByRole),
                    Some(role) => bail!("{BY_PIXIE}{role}: roles must be plain names"),
                    None => {
                        let disk = find_device(present, device).with_context(|| {
                            format!("disko.devices.disk.{name}.device is {device}")
                        })?;
                        (
                            name.clone(),
                            Source::Fixed {
                                device: device.to_string(),
                                disk: disk.name.clone(),
                            },
                        )
                    }
                },
            }
        };
        if !roles.insert(role.clone()) {
            bail!("two disko disks use the role {role}");
        }
        slots.push(Slot {
            name: name.clone(),
            role,
            source,
        });
    }
    Ok(slots)
}

/// The target disk a device path in the configuration means.
fn find_device<'a>(disks: &'a [Disk], device: &str) -> Result<&'a Disk> {
    let kernel_name = device
        .strip_prefix("/dev/")
        .filter(|name| is_plain_name(name));
    let found = if let Some(id) = device.strip_prefix(BY_ID) {
        disks.iter().find(|d| d.by_id.iter().any(|b| b == id))
    } else if let Some(name) = kernel_name {
        disks.iter().find(|d| d.name == name)
    } else {
        bail!("nix-pixie can only find disks named {BY_ID}..., {BY_PIXIE}<role> or /dev/<name>")
    };
    found.context("which is not on this target")
}

/// The disk the operator means by `spec`: a kernel name (`nvme0n1`,
/// `/dev/nvme0n1`), a `/dev/disk/by-id` name (with or without the
/// directory), or a serial number.
pub fn find<'a>(disks: &'a [Disk], spec: &str) -> Result<&'a Disk> {
    let spec = spec.trim();
    let matching: Vec<&Disk> = if let Some(id) = spec.strip_prefix(BY_ID) {
        disks
            .iter()
            .filter(|d| d.by_id.iter().any(|b| b == id))
            .collect()
    } else {
        let name = spec.strip_prefix("/dev/").unwrap_or(spec);
        disks
            .iter()
            .filter(|d| {
                d.name == name
                    || d.by_id.iter().any(|b| b == spec)
                    || d.serial.as_deref() == Some(spec)
            })
            .collect()
    };
    match matching.as_slice() {
        [disk] => Ok(disk),
        [] => {
            let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();
            bail!("the target has no disk {spec}; it has {}", names.join(", "))
        }
        many => {
            let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
            bail!("{spec} matches several disks: {}", names.join(", "))
        }
    }
}

/// Assigns a disk to every slot: fixed devices first, then the operator's
/// `requested` disks (by role), then pinned disks keep their recorded disk,
/// and a last slot with a single free disk left gets that one. Anything
/// still open is [`Resolution::Ask`].
pub fn resolve(
    slots: &[Slot],
    disks: &[Disk],
    requested: &BTreeMap<String, String>,
) -> Result<Resolution> {
    for role in requested.keys() {
        match slots.iter().find(|s| &s.role == role) {
            None => bail!("--disk {role}=...: no such role"),
            Some(Slot {
                source: Source::Fixed { device, .. },
                name,
                ..
            }) => bail!("--disk {role}=...: disko.devices.disk.{name}.device is fixed to {device}"),
            Some(_) => {}
        }
    }

    let mut assigned = Assigned::default();
    let by_name = |name: &str| disks.iter().find(|d| d.name == name);

    for slot in slots {
        if let Source::Fixed { disk, .. } = &slot.source {
            assigned.take(slot, by_name(disk).context("the disk disappeared")?)?;
        }
    }
    for slot in slots {
        if let Some(spec) = requested.get(&slot.role) {
            let disk = find(disks, spec).with_context(|| format!("--disk {}", slot.role))?;
            assigned.take(slot, disk)?;
        }
    }
    for slot in slots {
        if let Source::Pinned {
            recorded: Some(disk),
        } = &slot.source
            && !assigned.chosen.contains_key(&slot.name)
        {
            let disk = by_name(disk).context("the disk disappeared")?;
            assigned.take(slot, disk).with_context(|| {
                format!(
                    "{} can't go onto the disk recorded for it, {disk}; choose another with \
                     --disk {}=...",
                    slot.role,
                    slot.role,
                    disk = sanitise(&disk.name, 64)
                )
            })?;
        }
    }

    let open: Vec<&Slot> = slots
        .iter()
        .filter(|s| !assigned.chosen.contains_key(&s.name))
        .collect();
    let free: Vec<&Disk> = disks
        .iter()
        .filter(|d| d.installable() && !assigned.taken.contains_key(&d.name))
        .collect();
    match (open.as_slice(), free.as_slice()) {
        ([], _) => {}
        ([slot], [disk]) => assigned.take(slot, disk)?,
        ([slot, ..], []) => bail!("no free disk left for {}", slot.role),
        ([slot, ..], free) => {
            return Ok(Resolution::Ask {
                role: slot.role.clone(),
                candidates: free.iter().map(|d| d.name.clone()).collect(),
            });
        }
    }

    slots
        .iter()
        .map(|slot| {
            let disk = assigned
                .chosen
                .get(&slot.name)
                .and_then(|name| by_name(name))
                .context("a slot was left without a disk")?;
            let pin = match slot.source {
                Source::Pinned { .. } => Some(format!(
                    "{BY_ID}{}",
                    stable_id(disk).with_context(|| format!(
                        "pinned mode records /dev/disk/by-id names, and {} has none; \
                         use {BY_PIXIE}{} (agnostic mode) instead",
                        disk.name, slot.role
                    ))?
                )),
                _ => None,
            };
            Ok(Choice {
                name: slot.name.clone(),
                role: slot.role.clone(),
                disk: disk.name.clone(),
                pin,
                link: slot.source == Source::ByRole,
            })
        })
        .collect::<Result<_>>()
        .map(Resolution::Done)
}

/// Disks given out so far.
#[derive(Default)]
struct Assigned {
    /// Kernel name -> role.
    taken: BTreeMap<String, String>,
    /// Slot name -> kernel name.
    chosen: BTreeMap<String, String>,
}

/// Why nix-pixie may not install onto `disk`, for the operator, if it may
/// not (see [`Disk::installable`]). The target's own words are sanitised.
pub fn unusable(disk: &Disk) -> Option<String> {
    if disk.installable() {
        return None;
    }
    let name = sanitise(&disk.name, 64);
    Some(if disk.size_bytes == 0 {
        format!("{name} has no medium")
    } else if disk.read_only {
        format!("{name} is read-only")
    } else {
        let mounts: Vec<String> = disk.mounts.iter().map(|m| sanitise(m, 128)).collect();
        format!(
            "{name} is in use ({}), and not by an earlier attempt of this installer: it may \
             be the installer's own disk. If you mounted it yourself, unmount it on the \
             target, or boot the installer again",
            mounts.join(", ")
        )
    })
}

impl Assigned {
    fn take(&mut self, slot: &Slot, disk: &Disk) -> Result<()> {
        if let Some(reason) = unusable(disk) {
            bail!("{reason} ({} would go there)", slot.role);
        }
        if let Some(other) = self.taken.insert(disk.name.clone(), slot.role.clone()) {
            bail!("{} is chosen for both {other} and {}", disk.name, slot.role);
        }
        self.chosen.insert(slot.name.clone(), disk.name.clone());
        Ok(())
    }
}

/// The `/dev/disk/by-id` name to record for `disk`: the most descriptive
/// one (model and serial) over opaque ones like `wwn-` or `nvme-eui.`.
pub fn stable_id(disk: &Disk) -> Option<&str> {
    const PREFERRED: [&str; 6] = ["nvme-", "ata-", "virtio-", "mmc-", "usb-", "scsi-"];
    let opaque = |id: &str| {
        id.starts_with("wwn-")
            || id.contains("-eui.")
            || id.starts_with("nvme-nvme.")
            || id.starts_with("nvme-uuid.")
    };
    disk.by_id
        .iter()
        .min_by_key(|id| {
            (
                opaque(id),
                PREFERRED
                    .iter()
                    .position(|p| id.starts_with(p))
                    .unwrap_or(PREFERRED.len()),
                id.len(),
                id.as_str(),
            )
        })
        .map(String::as_str)
}

/// Names disko creates and then finds its devices by: GPT partition labels
/// (it mounts `/dev/disk/by-partlabel/<label>`) and ZFS pool names.
pub fn created_names(disko: &Disko) -> BTreeSet<String> {
    disko
        .disks
        .values()
        .flat_map(|d| d.part_labels.iter().cloned())
        .chain(disko.zpools.iter().cloned())
        .collect()
}

/// Refuses if a disk that is not being formatted already carries one of
/// `names`: disko could then mount or import the old partition or pool
/// instead of the one it just created, and the system would land on the
/// wrong disk. Typical cause: a disk from an earlier install with the same
/// layout is still attached. `Err` is a reason for the operator.
pub fn check_name_clashes(
    names: &BTreeSet<String>,
    formatting: &BTreeSet<&str>,
    disks: &[Disk],
) -> Result<(), String> {
    let clashes: Vec<String> = disks
        .iter()
        .filter(|d| !formatting.contains(d.name.as_str()))
        .filter_map(|disk| {
            let found: BTreeSet<&str> = disk
                .contents
                .iter()
                .flat_map(|c| {
                    let pool = (c.fstype.as_deref() == Some("zfs_member"))
                        .then_some(c.label.as_deref())
                        .flatten();
                    [c.partlabel.as_deref(), pool]
                })
                .flatten()
                .filter(|name| names.contains(*name))
                .collect();
            (!found.is_empty()).then(|| {
                let found: Vec<&str> = found.into_iter().collect();
                format!("{} ({})", disk.name, found.join(", "))
            })
        })
        .collect();
    if clashes.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} already carry partition labels or pool names this configuration creates. disko \
         finds its partitions and pools by name and could use the old ones, putting the system \
         on the wrong disk. Erase them (e.g. `wipefs --all` on the target) or detach those disks",
        clashes.join(", ")
    ))
}

/// The pinned-mode file: `{ "<disko disk>": "/dev/disk/by-id/..." }`.
pub fn pins_json(choices: &[Choice]) -> Result<Vec<u8>> {
    let pins: BTreeMap<&str, &str> = choices
        .iter()
        .filter_map(|c| Some((c.name.as_str(), c.pin.as_deref()?)))
        .collect();
    let mut json = serde_json::to_vec_pretty(&pins)?;
    json.push(b'\n');
    Ok(json)
}

/// After recording the pins, the configuration must use them.
pub fn verify_pins(disko: &Disko, choices: &[Choice], file: &str) -> Result<()> {
    for choice in choices {
        let Some(pin) = &choice.pin else { continue };
        let device = disko
            .disks
            .get(&choice.name)
            .and_then(|d| d.device.as_deref());
        if device != Some(pin.as_str()) {
            bail!(
                "disko.devices.disk.{}.device is {} after recording {pin} in {file}; \
                 read it with nix-pixie.lib.pinnedDisk ./{file} \"{}\"",
                choice.name,
                device.unwrap_or("unset"),
                choice.name
            );
        }
    }
    Ok(())
}
