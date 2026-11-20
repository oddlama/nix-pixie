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
