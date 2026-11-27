//! The controller's side of the store push: `nix copy` into the target's
//! disk, through the paired session.
//!
//! `nix copy --to unix://<socket>` talks the Nix daemon protocol to a socket
//! in a private directory. Every connection on it becomes a QUIC stream to the
//! target, where a `nix daemon --stdio` serves the store under the mounted
//! target filesystems (`crates/nix-pixie/src/store.rs`). Nothing is staged in
//! between, and the target never gets to read the controller's store.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_compression::tokio::bufread::ZstdDecoder;
use async_compression::tokio::write::ZstdEncoder;
use iroh::endpoint::Connection;
use nix_pixie_proto::ctrl::{ControllerMsg, StreamRequest, TargetMsg};
use nix_pixie_proto::frame::write_frame;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::link::Link;
use crate::net::{self, Route, sanitise};
use crate::state::private_dir;
use crate::{nix, nixlog};

/// What a finished push reports.
pub struct PushReport {
    pub fstype: String,
    pub copied: Copied,
}

/// What one `nix copy` to the target did.
#[derive(Debug, Clone, PartialEq)]
pub struct Copied {
    pub seconds: f64,
    /// How much was copied, for the operator (empty if nothing was).
    pub what: String,
}

/// When store streams are compressed (`--compression`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Compression {
    /// By the connection's path, for each stream: not on direct paths to
    /// nearby addresses (link-local or private ones, and any on one of the
    /// controller's networks: a USB cable, the LAN), which are faster than
    /// zstd on many a target's CPU; on relayed paths and to other
    /// addresses, where the link is the bottleneck.
    #[default]
    Auto,
    /// Always, with zstd.
    Zstd,
    /// Never.
    None,
}

/// The `--compression` in effect: [`Compression`] as a number.
static COMPRESSION: AtomicU8 = AtomicU8::new(0);

/// Sets when store streams are compressed, for the rest of the run.
pub fn set_compression(compression: Compression) {
    COMPRESSION.store(compression as u8, Ordering::Relaxed);
}

fn compression() -> Compression {
    match COMPRESSION.load(Ordering::Relaxed) {
        1 => Compression::Zstd,
        2 => Compression::None,
        _ => Compression::Auto,
    }
}

/// Whether a stream over `route` goes compressed under `compression`.
/// `networks`: the controller's own (an address, its prefix length), whose
/// hosts are nearby however they are numbered (a LAN with global IPv6).
#[must_use]
pub fn compresses(compression: Compression, route: &Route, networks: &[(IpAddr, u8)]) -> bool {
    match compression {
        Compression::Zstd => true,
        Compression::None => false,
        Compression::Auto => match route {
            Route::Direct(addr) => !is_nearby(addr.ip().to_canonical(), networks),
            Route::Relayed(_) | Route::Unknown => true,
        },
    }
}

/// Whether `ip` is on this link, a private network, or one of `networks`.
fn is_nearby(ip: IpAddr, networks: &[(IpAddr, u8)]) -> bool {
    let private = match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_unicast_link_local() || v6.is_unique_local() || v6.is_loopback(),
    };
    private
        || networks
            .iter()
            .any(|&(network, prefix)| same_network(ip, network, prefix))
}

/// Whether `a` and `b` share their first `prefix` bits. Not for a prefix of
/// 0, which every address would share.
fn same_network(a: IpAddr, b: IpAddr, prefix: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) if (1..=32).contains(&prefix) => {
            let mask = u32::MAX << (32 - u32::from(prefix));
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) if (1..=128).contains(&prefix) => {
            let mask = u128::MAX << (128 - u32::from(prefix));
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// This machine's networks: an address of each interface but loopback, and
/// its prefix length.
fn own_networks() -> Vec<(IpAddr, u8)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) => (IpAddr::V4(v4.ip), v4.prefixlen),
            if_addrs::IfAddr::V6(v6) => (IpAddr::V6(v6.ip), v6.prefixlen),
        })
        .collect()
}

/// Fails unless every path is valid in the local store, so a typo surfaces
/// before pairing instead of halfway through an install.
pub async fn check_local_paths(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let output = nix::command()
        .arg("path-info")
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
/// target.
pub async fn push(link: &Link, root: &str, paths: &[String]) -> Result<PushReport> {
    let fstype = prepare_disk_store(link, root).await?;
    warn_if_relayed(&link.connection());
    let request = StreamRequest::Store { root: root.into() };
    let copied = send(link, &request, paths).await?;
    Ok(PushReport { fstype, copied })
}

/// Tells the operator if a copy is about to go through a relay. Relays are
/// for finding a direct path; a whole system through one is slow, and
/// somebody else pays for the traffic.
pub fn warn_if_relayed(conn: &Connection) {
    if let Route::Relayed(relay) = net::route(conn) {
        warn!(%relay, "no direct path to the target (yet): copying through the relay, which is slow");
    }
}

/// Asks the target to check that the store under `root` is on a disk.
/// Returns that disk's filesystem type.
pub async fn prepare_disk_store(link: &Link, root: &str) -> Result<String> {
    let request = ControllerMsg::PrepareStore { root: root.into() };
    let reply = link
        .request(
            &request,
            Duration::from_secs(30),
            "the target to prepare its store",
        )
        .await?;
    match reply {
        TargetMsg::StoreReady { fstype } => Ok(sanitise(&fstype, 32)),
        TargetMsg::StoreRefused { reason } => {
            bail!(
                "the target refused to receive into {root}: {}",
                sanitise(&reason, 300)
            )
        }
        _ => bail!("unexpected reply to the store request"),
    }
}

/// Copies `paths` into the target store `request` names, with the progress
/// on the running step. If the connection is lost meanwhile, the copy
/// starts over once it is back, skipping what arrived.
pub async fn send(link: &Link, request: &StreamRequest, paths: &[String]) -> Result<Copied> {
    link.on_bulk_connection(async |conn| copy(conn, request, paths).await)
        .await
}

/// Runs `nix copy` for `paths` into the target store `request` names, over
/// `conn`.
async fn copy(conn: &Connection, request: &StreamRequest, paths: &[String]) -> Result<Copied> {
    // Private: the target's daemon runs as root and trusts whoever connects.
    let dir = private_dir()?;
    let socket = dir.join("store.sock");
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    let forwarder = tokio::spawn(forward(listener, conn.clone(), request.clone()));

    info!(?request, ?paths, "pushing");
    let started = Instant::now();
    let mut command = nix::command();
    // Paths the controller built itself carry no signatures, and the target
    // trusts the paired, approved controller anyway.
    command
        .args(["copy", "--no-check-sigs", "--to"])
        .arg(format!("unix://{}", socket.display()))
        .args(paths);
    let output = nixlog::run(command).await.context("running `nix copy`");
    forwarder.abort();
    let _ = std::fs::remove_dir_all(&dir);
    let output = output?;
    if !output.status.success() {
        bail!("`nix copy` to the target failed ({})", output.status);
    }
    let seconds = started.elapsed().as_secs_f64();
    let what = output.progress.outcome();
    // Nix's progress is only read on a live console.
    if what.is_empty() {
        info!(seconds, "pushed");
    } else {
        info!(seconds, copied = what, "pushed");
    }
    Ok(Copied { seconds, what })
}

/// Connects every client of the local socket to a new stream of the kind
/// `request` names.
async fn forward(listener: UnixListener, conn: Connection, request: StreamRequest) {
    let route = net::route(&conn);
    let networks = own_networks();
    let zstd = compresses(compression(), &route, &networks);
    info!(
        compressed = zstd,
        route = %route,
        "copying to the target{}",
        if zstd { " compressed (zstd)" } else { "" }
    );
    let mut tasks = JoinSet::new();
    while let Ok((mut local, _)) = listener.accept().await {
        let conn = conn.clone();
        // By the path now: a relayed one may have become direct.
        let zstd = compresses(compression(), &net::route(&conn), &networks);
        let request = if zstd {
            StreamRequest::Zstd(Box::new(request.clone()))
        } else {
            request.clone()
        };
        tasks.spawn(async move {
            let result = async {
                let (mut send, recv) = conn.open_bi().await?;
                write_frame(&mut send, &request).await?;
                let (up, down) = if zstd {
                    let mut decoder = ZstdDecoder::new(tokio::io::BufReader::new(recv));
                    decoder.multiple_members(true);
                    let mut remote = tokio::io::join(decoder, ZstdEncoder::new(send));
                    tokio::io::copy_bidirectional(&mut local, &mut remote).await?
                } else {
                    let mut remote = tokio::io::join(recv, send);
                    tokio::io::copy_bidirectional(&mut local, &mut remote).await?
                };
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
