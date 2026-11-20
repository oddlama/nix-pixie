//! Control messages on the `ctrl` stream after pairing succeeded.
//!
//! ```text
//! target                         controller
//! Hello                     ->
//! Inventory                 ->
//!                           <-   Approve | Reject
//!                           <-   RequestFacterReport        (after Approve)
//! FacterReport | FacterFailed ->
//!                           <-   PrepareDisks { disks }      (after Approve)
//! DisksReady | DisksRefused ->
//!                           <-   RunDisko { script }         (after DisksReady)
//! DiskoOutput*              ->
//! DiskoDone | DiskoFailed   ->
//!                           <-   PrepareStore { root }       (after Approve)
//! StoreReady | StoreRefused ->
//! ```
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

use serde::{Deserialize, Serialize};

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
    /// Output of the running disko script, as it comes.
    DiskoOutput { chunk: Vec<u8> },
    /// disko finished; the target filesystems are mounted.
    DiskoDone,
    /// disko could not run or failed.
    DiskoFailed { error: String },
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
    /// Check that these disks may be formatted, and link the ones disko
    /// addresses by role. Only after `Approve`, once the operator confirmed
    /// that their contents will be destroyed.
    PrepareDisks { disks: Vec<DiskAssignment> },
    /// Run this disko script (destroy, format, mount), already pushed into
    /// the live store. Only after `DisksReady`.
    RunDisko { script: String },
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
