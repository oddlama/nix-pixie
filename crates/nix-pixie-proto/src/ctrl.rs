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
//!                           <-   GenerateHostKey | ProvideHostKey (after Approve)
//! HostKey | HostKeyFailed   ->
//!                           <-   RunDisko { script }         (after DisksReady)
//! Output*                   ->
//! DiskoDone | DiskoFailed   ->
//!                           <-   PrepareStore { root }       (after Approve)
//! StoreReady | StoreRefused ->
//!                           <-   Install { root, system, .. } (after Approve)
//! Output*                   ->
//! InstallDone | InstallFailed ->
//!                           <-   PlaceFiles { root, files }  (root mounted)
//! FilesPlaced | FilesRefused ->
//!                           <-   RequestAddresses           (if found at a relay)
//! Addresses                 ->
//!                           <-   Exec { program, args, root } (for hooks)
//! ExecOutput*               ->
//! ExecExited | ExecFailed   ->
//!                           <-   Finish { root, zpools, then }
//! Finished | FinishFailed   ->
//! ```
//!
//! After `Finished`, the target reboots or powers off as soon as the
//! controller closes the connection.
//!
//! Every request but `Approve` and `Reject` gets exactly one final reply
//! (after any `Output` or `ExecOutput` frames), one at a time, and both
//! sides count the replies. If the connection breaks after `Approve`, the
//! target finishes what it was doing, keeps its last reply, and waits for
//! the same controller to come back on a new connection (ALPN
//! [`crate::ALPN_RESUME`]):
//!
//! ```text
//! target                         controller
//!                           <-   Resume { answered: n }
//! Resumed { answered: m }   ->
//! (its last reply, if m = n + 1)
//! ```
//!
//! With `m = n` the target never got the request the controller waits for
//! (if any), so the controller sends it again. A request is never carried
//! out twice.
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
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::inventory::Inventory;

/// Upper bound for control frames from the target, except facter reports.
/// Their content is untrusted, even after pairing.
pub const MAX_TARGET_FRAME: usize = 1024 * 1024;
/// Upper bound for a facter report frame. Real reports are a few hundred KiB.
pub const MAX_FACTER_FRAME: usize = 16 * 1024 * 1024;
/// Upper bound for control frames from the controller. Large enough for the
/// files hooks put into the installed system (`PlaceFiles`); the controller
/// is paired, so this guards against bugs rather than attacks.
pub const MAX_CONTROLLER_FRAME: usize = 16 * 1024 * 1024;

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
    /// The files are in place.
    FilesPlaced,
    /// Why the files could not be placed.
    FilesRefused { reason: String },
    /// The addresses of the target's own network interfaces, as requested.
    Addresses { addrs: Vec<IpAddr> },
    /// Output of the program `Exec` runs, as it comes: from its stderr, or
    /// else its stdout.
    ExecOutput { stderr: bool, chunk: Vec<u8> },
    /// The program `Exec` ran is done: its exit code, `None` if a signal
    /// ended it.
    ExecExited { code: Option<i32> },
    /// Why the program could not run.
    ExecFailed { error: String },
    /// The session goes on after `Resume`: how many requests the target
    /// has answered. If that is one more than the controller has replies
    /// for, the last reply follows.
    Resumed { answered: u64 },
    /// How far the target's clock was off the controller's time
    /// (milliseconds, ahead if positive), and whether it was set to it.
    Clock { off_ms: i64, set: bool },
    /// Why the clock could not be set.
    ClockFailed { error: String },
    /// The firmware boots the installed system next: the boot entry
    /// (`Boot####`, by its hex number), which comes first in the boot order
    /// now, and whether it was made for this, rather than there already.
    BootEntry { number: String, created: bool },
    /// Why the boot entry could not be made, or put first.
    BootEntryFailed { error: String },
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
    /// Put these files into the installed system, below `root` (mounted).
    /// Written by hooks; possibly secrets.
    PlaceFiles {
        root: String,
        files: Vec<PlacedFile>,
    },
    /// Push mode: keep this private host key (OpenSSH format, without a
    /// passphrase) for `Install`, as if it had been generated. Answered
    /// like `GenerateHostKey`. Only after `Approve`.
    ProvideHostKey { private: Secret },
    /// Send the addresses of the target's own network interfaces. A
    /// controller that found the target at a relay compares them with the
    /// address its connection goes to: that is the target itself, or a NAT
    /// in front of it. Only after `Approve`.
    RequestAddresses,
    /// Run `program`, a file in the Nix store the controller pushed it into,
    /// with `args`: without a shell, without input, and with nothing of the
    /// installer's environment, so it needs nothing but its closure. With
    /// `root`, inside the system installed there (as `nixos-enter` does),
    /// whose store has the program. For hooks; only after `Approve`.
    Exec {
        program: String,
        args: Vec<String>,
        root: Option<String>,
        /// Its stdin, e.g. a key; `None`: none.
        input: Option<Secret>,
    },
    /// First frame after the controller came back (ALPN
    /// [`crate::ALPN_RESUME`]): how many replies it has got.
    Resume { answered: u64 },
    /// Make the firmware boot the installed systemd-boot, whose EFI system
    /// partition is mounted at `esp`: a boot entry for it ("Linux Boot
    /// Manager", as `bootctl install` makes), first in the boot order. For
    /// boot loaders that make none themselves (lanzaboote), when the
    /// configuration allows writing EFI variables; otherwise the firmware
    /// may boot the installer again. Answered with `BootEntry`. Only after
    /// `Install`.
    AddBootEntry { esp: String },
    /// The controller's time (milliseconds since 1970, UTC). A target whose
    /// clock is off sets it to that, for whatever checks certificates or
    /// dates from now on: installers often boot without a clock, or with
    /// one months off. Answered with `Clock`. Only after `Approve`.
    SetClock { unix_ms: u64 },
}

/// Secret bytes: wiped from memory when dropped, never shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub Zeroizing<Vec<u8>>);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} secret bytes>", self.0.len())
    }
}

/// A file for the installed system, e.g. a secret a hook produced, or a
/// directory. Its contents are wiped from memory when dropped and never
/// shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedFile {
    /// Absolute path as the installed system sees it.
    pub path: String,
    /// Permission bits; the agent drops setuid, setgid and sticky.
    pub mode: u32,
    /// A directory, created if missing and given the mode and owner, rather
    /// than a file. Without contents.
    pub directory: bool,
    /// Its owner and group; `None`: root.
    pub user: Option<Account>,
    pub group: Option<Account>,
    pub contents: Zeroizing<Vec<u8>>,
}

impl PlacedFile {
    /// A file owned by root.
    #[must_use]
    pub fn file(path: String, mode: u32, contents: Zeroizing<Vec<u8>>) -> Self {
        Self {
            path,
            mode,
            directory: false,
            user: None,
            group: None,
            contents,
        }
    }
}

/// A user or group: by number, or by name in the installed system, which
/// only has its users and groups once it is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Account {
    Id(u32),
    Name(String),
}

impl fmt::Debug for PlacedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlacedFile")
            .field("path", &self.path)
            .field("mode", &format_args!("{:o}", self.mode))
            .field("directory", &self.directory)
            .field("user", &self.user)
            .field("group", &self.group)
            .field("contents", &format_args!("<{} bytes>", self.contents.len()))
            .finish()
    }
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
    /// The same, with the rest of the stream compressed with zstd, each
    /// direction one zstd frame, flushed whenever its writer waits.
    Zstd(Box<StreamRequest>),
}

/// Upper bound for a [`StreamRequest`] frame.
pub const MAX_STREAM_REQUEST: usize = 4096;
