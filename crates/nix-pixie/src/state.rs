//! Small status files: the agent's `/run/nix-pixie` directory and the
//! controller's optional code file. The console UI and tests read them. Writes
//! are atomic, so readers never see partial content, and the files are private
//! because the pairing code is a secret.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A private (0700) directory of status files, written atomically.
#[derive(Debug, Clone)]
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    pub fn create(path: PathBuf) -> Result<Self> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn write(&self, name: &str, contents: &str) -> Result<()> {
        write_private(&self.path.join(name), contents)
    }

    /// The file's contents without the trailing newline, if it exists.
    pub fn read(&self, name: &str) -> Option<String> {
        let contents = fs::read_to_string(self.path.join(name)).ok()?;
        Some(contents.strip_suffix('\n').unwrap_or(&contents).to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remove(&self, name: &str) {
        let _ = fs::remove_file(self.path.join(name));
    }
}

/// A fresh 0700 directory under `$XDG_RUNTIME_DIR` (else the temporary
/// directory), for things no other local user may reach: the store socket,
/// hooks' context and outputs. The caller removes it.
pub fn private_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let suffix: u64 = rand::random();
    let dir = base.join(format!("nix-pixie-{}-{suffix:016x}", std::process::id()));
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Like [`private_dir`], but in RAM, for what may be secrets, such as a
/// hook's outputs: in `$XDG_RUNTIME_DIR`, `/run/user/<uid>` or `/dev/shm`,
/// whichever is on tmpfs first.
pub fn private_ram_dir() -> Result<PathBuf> {
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").context("reading /proc/self/mountinfo")?;
    let uid = rustix::process::getuid().as_raw();
    let candidates = [
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        Some(PathBuf::from(format!("/run/user/{uid}"))),
        Some(PathBuf::from("/dev/shm")),
    ];
    let base = candidates
        .into_iter()
        .flatten()
        .filter(|p| p.is_absolute() && p.is_dir())
        .find(|p| in_ram(p, &mountinfo))
        .with_context(|| {
            format!(
                "hooks' outputs may be secrets, which must not touch a disk, but none of \
                 $XDG_RUNTIME_DIR, /run/user/{uid} and /dev/shm is on tmpfs"
            )
        })?;
    let suffix: u64 = rand::random();
    let dir = base.join(format!("nix-pixie-{}-{suffix:016x}", std::process::id()));
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Whether `path` is on a filesystem in RAM, by `mountinfo`.
pub fn in_ram(path: &Path, mountinfo: &str) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    crate::store::mounts(mountinfo)
        .filter(|(point, _)| path.starts_with(point))
        .max_by_key(|(point, _)| point.components().count())
        .is_some_and(|(_, fstype)| matches!(fstype.as_str(), "tmpfs" | "ramfs"))
}

/// Atomically replaces `path` with `contents` plus a newline, mode 0600.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    let name = path.file_name().context("path has no file name")?;
    let tmp = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    writeln!(file, "{contents}")?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
