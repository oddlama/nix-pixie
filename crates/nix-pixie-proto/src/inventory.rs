//! What a target reports about its hardware after pairing.
//!
//! The controller shows it for approval, matches it against the hints in the
//! user's flake, and later uses it to pick disks. It comes from the target and
//! is untrusted: sanitise strings before display, and validate anything that
//! ends up in a path or a script (see [`is_valid_disk_id`]).

use serde::{Deserialize, Serialize};

/// The target's hardware, sent once right after pairing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// Nix system double, e.g. `x86_64-linux`.
    pub system: String,
    pub hostname: String,
    pub dmi: Dmi,
    pub cpu: Cpu,
    pub memory_bytes: u64,
    pub firmware: Firmware,
    pub nics: Vec<Nic>,
    pub disks: Vec<Disk>,
}

/// SMBIOS identification; absent on many ARM boards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dmi {
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub uuid: Option<String>,
}

/// The CPU, as far as it matters for choosing a configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cpu {
    pub model: Option<String>,
    pub threads: u32,
}

/// Boot firmware, from efivars. Decides which bootloaders can work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firmware {
    pub uefi: bool,
    /// `None` when unknown (BIOS, or the variable is missing).
    pub secure_boot: Option<bool>,
    /// Secure Boot setup mode: keys can be enrolled.
    pub setup_mode: Option<bool>,
}

/// A physical network interface. Its MAC can match a machine's hints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nic {
    pub name: String,
    /// Lowercase, colon-separated.
    pub mac: String,
    pub link_up: bool,
}

/// A whole disk (not a partition) as lsblk reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags mirroring lsblk's columns"
)]
pub struct Disk {
    /// Kernel name, e.g. `nvme0n1`.
    pub name: String,
    /// Names in `/dev/disk/by-id` that point at this disk (not partitions).
    pub by_id: Vec<String>,
    pub size_bytes: u64,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// `nvme`, `sata`, `usb`, `virtio`, ...
    pub transport: Option<String>,
    pub rotational: bool,
    pub removable: bool,
    pub read_only: bool,
    /// `gpt` or `dos`, if there is one.
    pub partition_table: Option<String>,
    /// Signatures found on the disk itself and its partitions (and anything
    /// stacked on them, like LUKS or LVM).
    pub contents: Vec<Content>,
    /// Where things on it are mounted (`[SWAP]` for swap), e.g. the
    /// installer's own boot medium.
    pub mounts: Vec<String>,
    /// nix-pixie formatted it earlier during this boot of the installer, so
    /// whatever is mounted from it belongs to that attempt, and the next
    /// disko run unmounts it.
    pub formatted_this_boot: bool,
}

impl Disk {
    /// Something on it is mounted or used as swap.
    #[must_use]
    pub fn in_use(&self) -> bool {
        !self.mounts.is_empty()
    }

    /// Whether nix-pixie may install onto it. Not the installer's own boot
    /// medium or anything else in use, unless it was nix-pixie's own doing.
    #[must_use]
    pub fn installable(&self) -> bool {
        self.size_bytes > 0 && !self.read_only && (!self.in_use() || self.formatted_this_boot)
    }

    /// Whether installing onto this disk would destroy something.
    #[must_use]
    pub fn has_data(&self) -> bool {
        self.partition_table.is_some() || !self.contents.is_empty()
    }
}

/// Something found on or inside a disk: a partition, a filesystem, LUKS or
/// LVM. Shown to the operator before anything is wiped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    /// Kernel name of the device carrying it, e.g. `nvme0n1p2`.
    pub device: String,
    pub size_bytes: u64,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub partlabel: Option<String>,
}

/// Whether `id` is acceptable as a `/dev/disk/by-id` name. These end up in
/// generated disko configs and shell scripts, so only a conservative set of
/// characters passes.
#[must_use]
pub fn is_valid_disk_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'-'))
}

/// The Nix system double for a Rust `target_arch` on Linux.
#[must_use]
pub fn nix_system(arch: &str) -> String {
    let arch = match arch {
        "x86" => "i686",
        "arm" => "armv7l",
        other => other,
    };
    format!("{arch}-linux")
}
