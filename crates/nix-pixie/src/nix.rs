//! The operator's `nix`, as the controller runs it.

/// `nix` with the experimental features the controller's calls use
/// (`nix-command` and flakes), so they work also where the operator's
/// `nix.conf` doesn't enable them. Builds still go wherever the operator's
/// Nix sends them.
pub fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("nix");
    command.args(["--extra-experimental-features", "nix-command flakes"]);
    command
}
