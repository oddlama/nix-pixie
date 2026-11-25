//! Programs hooks run on the target (`nix-pixie target exec`): which may
//! run, and how the target runs them.
//!
//! Only files in the Nix store, which the controller pushes there with
//! their closure first: no shell, no `PATH` lookup, nothing of the
//! installer's environment. A program that runs on the controller's
//! machine runs the same on any target.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

const STORE: &str = "/nix/store/";
/// The installed system's `env`, inside it: to start the program there with
/// [`ENV`] and nothing else.
const SYSTEM_ENV: &str = "/nix/var/nix/profiles/system/sw/bin/env";
/// All of a program's environment. `PATH` leads nowhere: what a program
/// runs must come from its closure.
pub const ENV: [(&str, &str); 3] = [
    ("HOME", "/root"),
    ("LANG", "C.UTF-8"),
    ("PATH", "/var/empty"),
];

/// The store path `program` is in: `/nix/store/<hash>-sbctl` for
/// `/nix/store/<hash>-sbctl/bin/sbctl`. Refuses anything but a normalised
/// path to a file below one.
pub fn store_path(program: &str) -> Result<&str, String> {
    let refuse = || {
        Err(format!(
            "{program}: only programs in the Nix store run on the target, e.g. \
             ${{pkgs.sbctl}}/bin/sbctl, so they bring everything they need"
        ))
    };
    let Some(rest) = program.strip_prefix(STORE) else {
        return refuse();
    };
    // `components` skips `.` and repeated slashes: compare with what it
    // makes of the path.
    let components: PathBuf = Path::new(program).components().collect();
    let normal = components.as_os_str() == program
        && Path::new(program)
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    let Some((name, file)) = rest.split_once('/') else {
        return refuse();
    };
    // `env` would take `NAME=VALUE` for a variable, not the program.
    if !normal
        || name.is_empty()
        || file.is_empty()
        || program.ends_with('/')
        || program.contains('=')
    {
        return refuse();
    }
    let end = STORE.len() + name.len();
    program.get(..end).map_or_else(refuse, Ok)
}

/// The command that runs `program` with `args`: on the live system, or with
/// `root` inside the system installed there. Checks that the program is
/// there, as the controller pushed it.
pub fn command(program: &str, args: &[String], root: Option<&str>) -> Result<Command, String> {
    store_path(program)?;
    let mut command = match root {
        None => {
            if !Path::new(program).is_file() {
                return Err(format!("{program} isn't in the target's store"));
            }
            let mut command = Command::new(program);
            command.args(args).env_clear().envs(ENV);
            command
        }
        Some(root) => {
            let root = Path::new(root);
            if !root.join("etc/NIXOS").exists() {
                return Err(format!("{} holds no installed system yet", root.display()));
            }
            if !root.join(program.trim_start_matches('/')).is_file() {
                return Err(format!(
                    "{program} isn't in the store under {}",
                    root.display()
                ));
            }
            // nixos-enter mounts /dev, /proc and /sys and activates the
            // system (for /etc), with the agent's PATH for its own tools.
            let mut command = Command::new("nixos-enter");
            command
                .arg("--root")
                .arg(root)
                .args(["--silent", "--", SYSTEM_ENV, "-i"])
                .args(ENV.iter().map(|(key, value)| format!("{key}={value}")))
                .arg(program)
                .args(args);
            command
        }
    };
    command.stdin(Stdio::null());
    Ok(command)
}
