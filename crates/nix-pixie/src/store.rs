//! The target's side of the store push: a Nix daemon serving the store under
//! the mounted target filesystems (e.g. `/mnt`), one per QUIC stream.
//!
//! Paths go from the controller's store straight onto the target's disk. They
//! must never land in the live system's store, which lives in RAM on the
//! installer. [`check_root`] enforces that before anything is received.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_compression::tokio::bufread::ZstdDecoder;
use async_compression::tokio::write::ZstdEncoder;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

/// Filesystems that live in RAM, or can't hold a store.
const REFUSED_FSTYPES: &[&str] = &[
    "tmpfs", "ramfs", "overlay", "squashfs", "iso9660", "devtmpfs", "proc", "sysfs",
];

/// Whether `path` is a store path itself (`/nix/store/<hash>-<name>`), not a
/// file inside one or anything elsewhere.
pub fn is_store_path(path: &str) -> bool {
    const BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let Some((hash, name)) = path
        .strip_prefix("/nix/store/")
        .and_then(|base| base.split_at_checked(32))
    else {
        return false;
    };
    let Some(name) = name.strip_prefix('-') else {
        return false;
    };
    hash.bytes().all(|b| BASE32.contains(&b))
        && !name.is_empty()
        && name.len() <= 211
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(&b))
}

/// Checks that `<root>/nix/store` would be on a disk mounted under `root`.
/// Returns its filesystem type.
pub fn check_root(root: &str) -> Result<String, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading /proc/self/mountinfo: {e}"))?;
    check_root_in(root, &mountinfo)
}

pub fn check_root_in(root: &str, mountinfo: &str) -> Result<String, String> {
    let root_path = Path::new(root);
    if !root_path.is_absolute()
        || root_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("`{root}` is not an absolute, normalised path"));
    }
    if root_path == Path::new("/") {
        return Err("refusing to write into the live system's store".into());
    }
    let store = root_path.join("nix/store");
    let mount = mounts(mountinfo)
        .filter(|(point, _)| store.starts_with(point))
        .max_by_key(|(point, _)| point.components().count());
    let Some((point, fstype)) = mount else {
        return Err("no mount found for the store".into());
    };
    if !point.starts_with(root_path) {
        return Err(format!(
            "nothing is mounted at {root}; mount the target's filesystems first"
        ));
    }
    if REFUSED_FSTYPES.contains(&fstype.as_str()) {
        return Err(format!(
            "{} would be on {fstype} (mounted at {}), which is in RAM or can't hold a store",
            store.display(),
            point.display()
        ));
    }
    Ok(fstype)
}

/// (mount point, filesystem type) for every line of a mountinfo file.
pub fn mounts(mountinfo: &str) -> impl Iterator<Item = (PathBuf, String)> + '_ {
    mountinfo.lines().filter_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let point = left.split(' ').nth(4)?;
        let fstype = right.split(' ').next()?;
        Some((PathBuf::from(unescape(point)), fstype.to_string()))
    })
}

/// Undoes mountinfo's octal escapes (`\040` for a space, and so on).
pub fn unescape(field: &str) -> String {
    let mut rest = field.as_bytes();
    let mut out = Vec::with_capacity(rest.len());
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'\\'
            && let Some((digits, after)) = tail.split_first_chunk::<3>()
            && let Some(decoded) = octal(*digits)
        {
            out.push(decoded);
            rest = after;
        } else {
            out.push(byte);
            rest = tail;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Three octal digits as a byte (`\000` to `\377`).
fn octal(digits: [u8; 3]) -> Option<u8> {
    digits.iter().try_fold(0u8, |value, &digit| {
        let digit = digit.checked_sub(b'0').filter(|d| *d < 8)?;
        value.checked_mul(8)?.checked_add(digit)
    })
}

/// Runs `nix daemon --stdio` for the store under `root` and connects it to the
/// stream until either side is done. The controller is trusted: it was paired
/// and approved, and it pushes paths it built itself, which carry no
/// signatures.
///
/// `root: None` serves the live system's own store instead.
/// Copies `paths` with their closures from the binary caches `from` into
/// the store under `root`, trusting signatures by `keys`, with Nix's
/// output as `Output` frames. The first cache that has everything ends it;
/// what arrived from others stays. Returns `Substituted`; the outer
/// `Result` is about the control stream.
pub async fn substitute(
    stream: &mut crate::net::Stream,
    root: &str,
    from: &[String],
    keys: &[String],
    paths: &[String],
) -> Result<nix_pixie_proto::ctrl::TargetMsg> {
    use nix_pixie_proto::ctrl::TargetMsg;
    let failed = |error: String| Ok(TargetMsg::Substituted { error: Some(error) });
    if let Err(reason) = check_root(root) {
        return failed(reason);
    }
    let url = |u: &String| {
        !u.is_empty()
            && !u.starts_with('-')
            && u.bytes().all(|b| b.is_ascii_graphic())
            && [
                "https://",
                "http://",
                "file://",
                "s3://",
                "ssh://",
                "ssh-ng://",
            ]
            .iter()
            .any(|scheme| u.starts_with(scheme))
    };
    if from.is_empty() || !from.iter().all(url) {
        return failed("not a binary cache URL".into());
    }
    if !keys
        .iter()
        .all(|k| k.bytes().all(|b| b.is_ascii_graphic()) && !k.is_empty())
    {
        return failed("not a public key".into());
    }
    if paths.is_empty() || !paths.iter().all(|p| is_store_path(p)) {
        return failed("not store paths".into());
    }
    let mut errors = Vec::new();
    for cache in from {
        let mut command = tokio::process::Command::new("nix");
        command
            .args(["--extra-experimental-features", "nix-command"])
            .args(["copy", "--from", cache, "--to"])
            .arg(format!("local?root={root}"))
            .args(["--option", "trusted-public-keys", &keys.join(" ")])
            .args(paths);
        match crate::child::run("nix copy", command, stream).await? {
            Ok(()) => {
                info!(%cache, "substituted");
                return Ok(TargetMsg::Substituted { error: None });
            }
            Err(error) => errors.push(format!("{cache}: {error}")),
        }
    }
    failed(errors.join("; "))
}

/// With `zstd`, both directions are compressed (see
/// `nix_pixie_proto::ctrl::StreamRequest::Zstd`).
pub async fn serve(
    root: Option<&str>,
    recv: RecvStream,
    send: SendStream,
    zstd: bool,
) -> Result<()> {
    let (mut recv, mut send): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) = if zstd {
        let mut decoder = ZstdDecoder::new(tokio::io::BufReader::new(recv));
        decoder.multiple_members(true);
        (Box::new(decoder), Box::new(ZstdEncoder::new(send)))
    } else {
        (Box::new(recv), Box::new(send))
    };
    let store = root.map_or_else(
        || "the live store".to_string(),
        |root| format!("local?root={root}"),
    );
    let mut command = tokio::process::Command::new("nix");
    // `--force-trusted` needs `daemon-trust-override`.
    command.args([
        "--extra-experimental-features",
        "nix-command daemon-trust-override",
    ]);
    if root.is_some() {
        command.args(["--store", &store]);
    }
    let mut daemon = command
        .args(["daemon", "--stdio", "--force-trusted"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting `nix daemon` (is Nix installed?)")?;
    let mut stdin = daemon.stdin.take().context("daemon stdin")?;
    let mut stdout = daemon.stdout.take().context("daemon stdout")?;
    debug!(%store, "store connection opened");

    // Both directions stream through small fixed buffers; QUIC flow control
    // keeps the controller from getting ahead of the disk.
    let upload = async {
        let copied = tokio::io::copy(&mut recv, &mut stdin).await;
        // End of the controller's requests: let the daemon finish and exit.
        let _ = stdin.shutdown().await;
        drop(stdin);
        copied
    };
    let download = async {
        let copied = tokio::io::copy(&mut stdout, &mut send).await;
        // Ends the zstd frame, if any, and the stream.
        let _ = send.shutdown().await;
        copied
    };
    let (up, down) = tokio::join!(upload, download);
    let status = daemon.wait().await?;
    debug!(?up, ?down, %status, "store connection closed");
    if !status.success() {
        bail!("nix daemon exited with {status}");
    }
    info!(received = up.unwrap_or(0), "store connection done");
    Ok(())
}
