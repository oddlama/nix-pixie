//! The operator's `nix`, as the controller runs it.

use std::sync::atomic::{AtomicBool, Ordering};

static OFFLINE: AtomicBool = AtomicBool::new(false);

/// From now on, Nix neither substitutes nor fetches anything (`--offline`
/// on the controller): for stations without internet, where everything is
/// in the local store already.
pub fn offline() {
    OFFLINE.store(true, Ordering::Relaxed);
}

/// `nix` with the experimental features the controller's calls use
/// (`nix-command` and flakes), so they work also where the operator's
/// `nix.conf` doesn't enable them, and `--offline` if asked. Builds still go
/// wherever the operator's Nix sends them.
pub fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("nix");
    command.args(["--extra-experimental-features", "nix-command flakes"]);
    if OFFLINE.load(Ordering::Relaxed) {
        command.arg("--offline");
    }
    command
}
