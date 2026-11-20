//! Control messages on the `ctrl` stream after pairing succeeded.
//!
//! ```text
//! target                         controller
//! Hello                     ->
//! Inventory                 ->
//!                           <-   Approve | Reject
//!                           <-   RequestFacterReport        (after Approve)
//! FacterReport | FacterFailed ->
//!                           <-   PrepareStore { root }       (after Approve)
//! StoreReady | StoreRefused ->
//! ```
//!
//! After `StoreReady`, the controller opens one more bi-stream per Nix daemon
//! connection, each starting with a [`StreamRequest`] frame. The target runs
//! `nix daemon --stdio` for it, serving the store under `root`, so `nix copy`
//! on the controller writes straight into the target's disk.
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
}

/// First frame on every bi-stream the controller opens after the control
/// stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamRequest {
    /// Connect this stream to a Nix daemon serving the store under `root`.
    Store { root: String },
}

/// Upper bound for a [`StreamRequest`] frame.
pub const MAX_STREAM_REQUEST: usize = 4096;
