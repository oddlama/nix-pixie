//! Exit codes a program driving the controller can rely on: they say how
//! far an install got, above all whether anything on the target was erased.
//!
//! | code | meaning |
//! |------|---------|
//! | 0    | done |
//! | 1    | failed, for another reason (e.g. outside `install` and `pair`) |
//! | 2    | the command line is wrong |
//! | 3    | no pairing: no target found, the code was rejected, no connection |
//! | 4    | failed before anything was erased (checks, approval, evaluation, build, the disks' plan) |
//! | 5    | failed after formatting began: the target's disks hold a partial install |
//! | 6    | installed, but the boot check failed or timed out |
//! | 130  | interrupted (Ctrl-C, SIGTERM); the `exit` event says how far it got |

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// How far a command got. Only ever goes forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Phase {
    /// Not an install or a pairing.
    Other = 0,
    /// Checking what can be checked before pairing.
    Checking = 1,
    Pairing = 2,
    /// Paired: approval, evaluation, build, the disks' plan.
    Paired = 3,
    /// Formatting began.
    Erasing = 4,
    /// The target reboots or powers off; the boot check runs.
    Installed = 5,
}

impl Phase {
    /// Its name, for the `exit` event.
    pub fn name(self) -> &'static str {
        match self {
            Self::Other => "other",
            Self::Checking => "checking",
            Self::Pairing => "pairing",
            Self::Paired => "paired",
            Self::Erasing => "erasing",
            Self::Installed => "installed",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Checking,
            2 => Self::Pairing,
            3 => Self::Paired,
            4 => Self::Erasing,
            5 => Self::Installed,
            _ => Self::Other,
        }
    }
}

static PHASE: AtomicU8 = AtomicU8::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The command reached `phase`.
pub fn reached(phase: Phase) {
    PHASE.fetch_max(phase as u8, Ordering::Relaxed);
}

/// How far the command got.
pub fn phase() -> Phase {
    Phase::from_u8(PHASE.load(Ordering::Relaxed))
}

/// The operator interrupted the command.
pub fn interrupted() {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// The exit code for a command that failed, by how far it got.
pub fn failure() -> u8 {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return 130;
    }
    match phase() {
        Phase::Other => 1,
        Phase::Pairing => 3,
        Phase::Checking | Phase::Paired => 4,
        Phase::Erasing => 5,
        Phase::Installed => 6,
    }
}
