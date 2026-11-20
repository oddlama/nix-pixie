//! The controller's side of the store push: `nix copy` into the target's
//! disk, through the paired session.
//!
//! `nix copy --to unix://<socket>` talks the Nix daemon protocol to a socket
//! in a private directory. Every connection on it becomes a QUIC stream to the
//! target, where a `nix daemon --stdio` serves the store under the mounted
//! target filesystems (`crates/nix-pixie/src/store.rs`). Nothing is staged in
//! between, and the target never gets to read the controller's store.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iroh::endpoint::Connection;
use nix_pixie_proto::ctrl::{ControllerMsg, MAX_TARGET_FRAME, StreamRequest, TargetMsg};
use nix_pixie_proto::frame::{read_frame, write_frame};
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::net::{Stream, sanitise};

pub struct PushReport {
    pub fstype: String,
    pub seconds: f64,
}

/// Fails unless every path is valid in the local store, so a typo surfaces
/// before pairing instead of halfway through an install.
pub async fn check_local_paths(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let output = tokio::process::Command::new("nix")
        .args(["--extra-experimental-features", "nix-command", "path-info"])
        .args(paths)
        .output()
        .await
        .context("running `nix path-info` (is Nix installed?)")?;
    if !output.status.success() {
        bail!(
            "not in the local store: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Copies `paths` (with their closures) into the store under `root` on the
/// target. `ctrl` is the session's control stream.
pub async fn push(
    conn: &Connection,
    ctrl: &mut Stream,
    root: &str,
    paths: &[String],
) -> Result<PushReport> {
    write_frame(ctrl, &ControllerMsg::PrepareStore { root: root.into() }).await?;
    let reply = tokio::time::timeout(Duration::from_secs(30), read_frame(ctrl, MAX_TARGET_FRAME))
        .await
        .context("timed out waiting for the target to prepare its store")??;
    let fstype = match reply {
        TargetMsg::StoreReady { fstype } => sanitise(&fstype, 32),
        TargetMsg::StoreRefused { reason } => {
            bail!(
                "the target refused to receive into {root}: {}",
                sanitise(&reason, 300)
            )
        }
        _ => bail!("unexpected reply to the store request"),
    };

    let dir = private_dir()?;
    let socket = dir.join("store.sock");
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    let forwarder = tokio::spawn(forward(listener, conn.clone(), root.to_string()));

    info!(%root, %fstype, ?paths, "pushing");
    let started = Instant::now();
    let status = tokio::process::Command::new("nix")
        // Paths the controller built itself carry no signatures, and the
        // target trusts the paired, approved controller anyway.
        .args([
            "--extra-experimental-features",
            "nix-command",
            "copy",
            "--no-check-sigs",
            "--to",
        ])
        .arg(format!("unix://{}", socket.display()))
        .args(paths)
        .status()
        .await
        .context("running `nix copy`");
    forwarder.abort();
    let _ = std::fs::remove_dir_all(&dir);
    let status = status?;
    if !status.success() {
        bail!("`nix copy` to the target failed ({status})");
    }
    let seconds = started.elapsed().as_secs_f64();
    info!(seconds, "pushed");
    Ok(PushReport { fstype, seconds })
}

/// Connects every client of the local socket to a new store stream.
async fn forward(listener: UnixListener, conn: Connection, root: String) {
    let mut tasks = JoinSet::new();
    while let Ok((mut local, _)) = listener.accept().await {
        let conn = conn.clone();
        let root = root.clone();
        tasks.spawn(async move {
            let result = async {
                let (mut send, recv) = conn.open_bi().await?;
                write_frame(&mut send, &StreamRequest::Store { root }).await?;
                let mut remote = tokio::io::join(recv, send);
                let (up, down) = tokio::io::copy_bidirectional(&mut local, &mut remote).await?;
                debug!(up, down, "store connection done");
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = result {
                warn!("store connection: {e:#}");
            }
        });
        while tasks.try_join_next().is_some() {}
    }
}

/// A fresh 0700 directory for the socket. The target's daemon runs as root
/// and trusts whoever connects, so no other local user may reach it.
fn private_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let suffix: u64 = rand::random();
    let dir = base.join(format!("nix-pixie-{}-{suffix:016x}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}
