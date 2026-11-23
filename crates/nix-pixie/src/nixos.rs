//! The target's side of turning formatted disks into a bootable NixOS: the
//! SSH host key (pull mode), `nixos-install`, and leaving the disks ready for
//! the first boot.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use nix_pixie_proto::ctrl::PlacedFile;
use tokio::process::Command;
use tracing::info;

use crate::child;
use crate::disko::is_plain_name;
use crate::net::Stream;
use crate::state::StateDir;
use crate::store::{check_root, is_store_path};

/// Where the generated key waits for the install, inside the state
/// directory (tmpfs, 0700).
const HOST_KEY_DIR: &str = "host-key";
const HOST_KEY: &str = "ssh_host_ed25519_key";

/// Generates a fresh ed25519 host key, replacing any earlier one, and
/// returns its public half.
pub async fn generate_host_key(state: &StateDir, comment: &str) -> Result<String> {
    let dir = state.path().join(HOST_KEY_DIR);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let key = dir.join(HOST_KEY);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .status()
        .await
        .context("running ssh-keygen (is OpenSSH installed?)")?;
    if !status.success() {
        bail!("ssh-keygen failed ({status})");
    }
    let public = std::fs::read_to_string(key.with_extension("pub"))
        .context("reading the generated public key")?;
    info!("generated the host key");
    Ok(public.trim().to_string())
}

/// Push mode: keeps `private` where a generated key would be, for
/// `install`, and returns its public half. Refuses anything `ssh-keygen`
/// can't read without a passphrase.
pub async fn store_host_key(state: &StateDir, private: &[u8]) -> Result<String> {
    let dir = state.path().join(HOST_KEY_DIR);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let key = dir.join(HOST_KEY);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key)
        .context("writing the host key")?;
    std::io::Write::write_all(&mut file, private)?;
    drop(file);
    let output = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running ssh-keygen (is OpenSSH installed?)")?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "not an OpenSSH private key without a passphrase: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let public = String::from_utf8_lossy(&output.stdout).trim().to_string();
    std::fs::write(key.with_extension("pub"), format!("{public}\n"))
        .context("writing the public host key")?;
    info!("stored the pushed host key");
    Ok(public)
}

/// `path` (absolute, as the installed system sees it) below `root`. Refuses
/// anything that could leave `root`.
pub fn below_root(root: &str, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path)
        .strip_prefix("/")
        .with_context(|| format!("{path} is not absolute"))?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("{path} is not a plain path");
    }
    Ok(Path::new(root).join(relative))
}

/// Copies the generated key to `path` in the installed system: private key
/// 0600, public key 0644, like `sshd-keygen` makes them.
fn install_host_key(state: &StateDir, root: &str, path: &str) -> Result<()> {
    let source = state.path().join(HOST_KEY_DIR).join(HOST_KEY);
    if !source.exists() {
        bail!("no host key was generated in this session");
    }
    let target = below_root(root, path)?;
    if let Some(dir) = target.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let public_target = PathBuf::from(format!("{}.pub", target.display()));
    for (from, to, mode) in [
        (source.clone(), target, 0o600),
        (source.with_extension("pub"), public_target, 0o644),
    ] {
        let contents =
            std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        let _ = std::fs::remove_file(&to);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&to)
            .with_context(|| format!("creating {}", to.display()))?;
        std::io::Write::write_all(&mut file, &contents)
            .with_context(|| format!("writing {}", to.display()))?;
    }
    info!(%path, "installed the host key");
    Ok(())
}

/// Installs `system` (already in the store under `root`): the host key if
/// asked for, then `nixos-install`, which sets the system profile and
/// installs the boot loader. Outer `Result`: the control stream; inner: the
/// outcome, as a reason for the operator.
pub async fn install(
    state: &StateDir,
    stream: &mut Stream,
    root: &str,
    system: &str,
    host_key_path: Option<&str>,
) -> Result<Result<(), String>> {
    if let Err(reason) = check_root(root) {
        return Ok(Err(reason));
    }
    if !is_store_path(system) {
        return Ok(Err("not a store path".into()));
    }
    let valid = Command::new("nix-store")
        .args([
            "--store",
            &format!("local?root={root}"),
            "--check-validity",
            system,
        ])
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !valid {
        return Ok(Err(format!("{system} is not in the store under {root}")));
    }
    if let Some(path) = host_key_path
        && let Err(e) = install_host_key(state, root, path)
    {
        return Ok(Err(format!("{e:#}")));
    }
    info!(%root, %system, "running nixos-install");
    let mut command = Command::new("nixos-install");
    command.args([
        "--root",
        root,
        "--system",
        system,
        "--no-root-passwd",
        "--no-channel-copy",
    ]);
    child::run("nixos-install", command, stream).await
}

/// Puts hook-written files into the installed system below `root`: root-owned,
/// with their permission bits minus setuid, setgid and sticky. Refuses paths
/// that leave `root`, also through symlinked directories inside it (NixOS's
/// `/etc/static` points outside, for one). `Err` is a reason for the operator.
pub fn place_files(root: &str, files: &[PlacedFile]) -> Result<(), String> {
    check_root(root)?;
    let real_root = std::fs::canonicalize(root).map_err(|e| format!("{root}: {e}"))?;
    for file in files {
        place_file(&real_root, root, file).map_err(|e| format!("{}: {e:#}", file.path))?;
    }
    info!(%root, files = files.len(), "placed files");
    Ok(())
}

/// One file of [`place_files`], below `root` (whose canonical path is
/// `real_root`); the mount check is the caller's.
pub fn place_file(real_root: &Path, root: &str, file: &PlacedFile) -> Result<()> {
    let target = below_root(root, &file.path)?;
    let dir = target.parent().context("no parent directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    if !std::fs::canonicalize(dir)?.starts_with(real_root) {
        bail!("leads outside the installed system (through a symlink)");
    }
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => bail!("is a directory"),
        Ok(_) => std::fs::remove_file(&target)?,
        Err(_) => {}
    }
    let mode = file.mode & 0o777;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&target)?;
    std::io::Write::write_all(&mut out, &file.contents)?;
    // Exactly these bits, whatever the umask.
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Unmounts everything under `root` and exports `zpools`, so the first boot
/// imports them without force. `Err` is a reason for the operator.
pub async fn finish(root: &str, zpools: &[String]) -> Result<(), String> {
    if let Some(pool) = zpools.iter().find(|p| !is_plain_name(p)) {
        return Err(format!("{pool:?} is not a pool name"));
    }
    run(Command::new("sync")).await?;
    let mut umount = Command::new("umount");
    umount.args(["--recursive", root]);
    run(umount).await?;
    for pool in zpools {
        let mut export = Command::new("zpool");
        export.args(["export", pool]);
        run(export).await?;
    }
    info!(%root, ?zpools, "ready to boot");
    Ok(())
}

/// Runs a short command, turning failure into a reason with its stderr.
async fn run(mut command: Command) -> Result<(), String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("running {program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
