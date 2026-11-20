//! The controller's long-term iroh identity.
//!
//! Stored as 64 hex characters in a 0600 file. Targets pin the matching
//! endpoint id after pairing, and baked installer images will carry it.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use iroh::SecretKey;

use nix_pixie_proto::hex;

/// `$XDG_STATE_HOME/nix-pixie/identity`, falling back to `~/.local/state`.
pub fn default_path() -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("neither XDG_STATE_HOME nor HOME is set; pass --identity")?;
    Ok(state.join("nix-pixie").join("identity"))
}

pub fn load_or_create(path: &Path) -> Result<SecretKey> {
    match load(path) {
        Err(e)
            if e.downcast_ref::<std::io::Error>().map(std::io::Error::kind)
                == Some(ErrorKind::NotFound) =>
        {
            create(path)
        }
        other => other,
    }
}

fn load(path: &Path) -> Result<SecretKey> {
    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is accessible by other users (mode {:o}); run `chmod 600` on it",
            path.display(),
            mode & 0o777
        );
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes: [u8; 32] = hex::decode(text.trim())
        .and_then(|b| b.try_into().ok())
        .with_context(|| format!("{} does not contain a 64-character hex key", path.display()))?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn create(path: &Path) -> Result<SecretKey> {
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let key = SecretKey::generate();
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        // Another process created it first; use theirs.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return load(path),
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    };
    writeln!(file, "{}", hex::encode(&key.to_bytes()))?;
    file.sync_all()?;
    tracing::info!(path = %path.display(), id = %key.public(), "created controller identity");
    Ok(key)
}
