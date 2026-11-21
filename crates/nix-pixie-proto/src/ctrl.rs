//! Control messages on the `ctrl` stream after pairing succeeded.
//!
//! ```text
//! target                         controller
//! Hello                     ->
//! Inventory                 ->
//!                           <-   Approve | Reject
//!                           <-   RequestFacterReport        (after Approve)
//! FacterReport | FacterFailed ->
//!                           <-   PrepareDisks { disks, labels } (after Approve)
//! DisksReady | DisksRefused ->
//!                           <-   GenerateHostKey             (after Approve)
//! HostKey | HostKeyFailed   ->
//!                           <-   RunDisko { script }         (after DisksReady)
//! Output*                   ->
//! DiskoDone | DiskoFailed   ->
//!                           <-   PrepareStore { root }       (after Approve)
//! StoreReady | StoreRefused ->
//!                           <-   Install { root, system, .. } (after Approve)
//! Output*                   ->
//! InstallDone | InstallFailed ->
//!                           <-   Finish { root, zpools, then }
//! Finished | FinishFailed   ->
//! ```
//!
//! After `Finished`, the target reboots or powers off as soon as the
//! controller closes the connection.
//!
//! The controller may open more bi-streams after `Approve`, one per Nix
//! daemon connection, each starting with a [`StreamRequest`] frame. The
//! target runs `nix daemon --stdio` for it: serving the live system's store
//! (for the disko script, which runs from there) or, after `StoreReady`, the
//! store under `root`, so `nix copy` on the controller writes straight into
//! the target's disk.
//!
//! The target's `Hello` also makes the end of pairing observable: when it
//! arrives, the controller knows the target finished pairing and can close the
//! connection without cutting it off mid-handshake.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::inventory::Inventory;

/// Upper bound for control frames from the target, except facter reports.
/// Their content is untrusted, even after pairing.
pub const MAX_TARGET_FRAME: usize = 1024 * 1024;
/// Upper bound for a facter report frame. Real reports are a few hundred KiB.
pub const MAX_FACTER_FRAME: usize = 16 * 1024 * 1024;
/// Upper bound for control frames from the controller.
pub const MAX_CONTROLLER_FRAME: usize = 64 * 1024;

/// Messages the target sends to the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetMsg {
    /// First message after pairing.
    Hello {
        /// Human-readable name, e.g. the installer's hostname. Untrusted:
        /// sanitise before display.
        label: String,
    },
    /// Second message after pairing.
    Inventory(Box<Inventory>),
    /// A nixos-facter report (JSON), as requested.
    FacterReport { json: Vec<u8> },
    /// Why there is no facter report.
    FacterFailed { error: String },
    /// The store under the requested root can receive paths.
    StoreReady { fstype: String },
    /// Why the store under the requested root can't receive paths.
    StoreRefused { reason: String },
    /// The disks are checked and linked; disko may run.
    DisksReady,
    /// Why the target won't let disko touch these disks.
    DisksRefused { reason: String },
    /// Output of the running disko script or `nixos-install`, as it comes.
    Output { chunk: Vec<u8> },
    /// disko finished; the target filesystems are mounted.
    DiskoDone,
    /// disko could not run or failed.
    DiskoFailed { error: String },
    /// The generated SSH host key's public half, in OpenSSH format.
    HostKey { public: String },
    /// Why there is no host key.
    HostKeyFailed { error: String },
    /// `nixos-install` finished: the system boots from the target's disks.
    InstallDone,
    /// `nixos-install` could not run or failed.
    InstallFailed { error: String },
    /// Unmounted and exported; the target shuts down once the controller is
    /// gone. `live_host_key`: the installer's own SSH host key, if it has
    /// one, so the controller doesn't mistake it for the installed system's
    /// while the target reboots.
    Finished { live_host_key: Option<String> },
    /// Why the disks could not be left ready to boot.
    FinishFailed { error: String },
}

/// Messages the controller sends to the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerMsg {
    /// The operator approved this machine, optionally as one of the machines
    /// in their flake. The target shows it; nothing destructive happens yet.
    Approve { machine: Option<String> },
    /// The operator rejected this machine. The target goes back to pairing.
    Reject { reason: String },
    /// Run nixos-facter and send the report. Only after `Approve`.
    RequestFacterReport,
    /// Check that a Nix store under `root` (e.g. `/mnt`, the mounted target
    /// filesystems) can receive paths. Only after `Approve`.
    PrepareStore { root: String },
    /// Check that these disks may be formatted and that no other disk
    /// carries any of `labels` (GPT partition labels and ZFS pool names
    /// disko will create, and then find by name), and link the disks disko
    /// addresses by role. Only after `Approve`, once the operator confirmed
    /// that their contents will be destroyed.
    PrepareDisks {
        disks: Vec<DiskAssignment>,
        labels: Vec<String>,
    },
    /// Run this disko script (destroy, format, mount), already pushed into
    /// the live store, with `key_files` in place for it. Only after
    /// `DisksReady`.
    RunDisko {
        script: String,
        key_files: Vec<KeyFile>,
    },
    /// Generate an ed25519 SSH host key for the installed system (pull
    /// mode). It stays on the installer's tmpfs until `Install`.
    GenerateHostKey,
    /// Install the system, already pushed into the store under `root`: put
    /// the generated host key at `host_key_path` (relative to `root`, if
    /// given), then run `nixos-install`, which sets the system profile and
    /// installs the boot loader.
    Install {
        root: String,
        system: String,
        host_key_path: Option<String>,
    },
    /// Leave the disks ready to boot (unmount `root`, export `zpools`), then
    /// shut down as `then` says once the controller disconnects.
    Finish {
        root: String,
        zpools: Vec<String>,
        then: Shutdown,
    },
}

/// A key disko reads from a file on the target while formatting encrypted
/// devices (e.g. a LUKS `passwordFile`). The agent keeps it in RAM only for
/// the disko run. The contents are wiped from memory when dropped and never
/// shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    /// Absolute path on the target, under `/tmp` or `/run`.
    pub path: String,
    pub contents: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyFile")
            .field("path", &self.path)
            .field("contents", &format_args!("<{} bytes>", self.contents.len()))
            .finish()
    }
}

/// How the target goes down after the install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shutdown {
    /// Boot the installed system.
    Reboot,
    /// Power off, e.g. to remove the installer medium first.
    PowerOff,
}

/// A disk the disko script is going to format, as the operator chose it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskAssignment {
    /// What the disk is for: its `/dev/disk/by-pixie` role in agnostic
    /// mode, else the disko disk's name.
    pub role: String,
    /// Kernel name from the inventory, e.g. `nvme0n1`.
    pub device: String,
    /// Link `/dev/disk/by-pixie/<role>` to it, because the disko config
    /// addresses the disk that way (agnostic mode).
    pub link: bool,
}

/// First frame on every bi-stream the controller opens after the control
/// stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamRequest {
    /// Connect this stream to a Nix daemon serving the store under `root`.
    Store { root: String },
    /// Connect this stream to a Nix daemon serving the live system's store.
    /// It lives in RAM on an installer, so only small closures belong here.
    LiveStore,
}

/// Upper bound for a [`StreamRequest`] frame.
pub const MAX_STREAM_REQUEST: usize = 4096;
