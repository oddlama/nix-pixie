//! The target's side of the store push: a Nix daemon serving the store under
//! the mounted target filesystems (e.g. `/mnt`), one per QUIC stream.
//!
//! Paths go from the controller's store straight onto the target's disk. They
//! must never land in the live system's store, which lives in RAM on the
//! installer. [`check_root`] enforces that before anything is received.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

/// Filesystems that live in RAM, or can't hold a store.
const REFUSED_FSTYPES: &[&str] = &[
    "tmpfs", "ramfs", "overlay", "squashfs", "iso9660", "devtmpfs", "proc", "sysfs",
];

/// Checks that `<root>/nix/store` would be on a disk mounted under `root`.
/// Returns its filesystem type.
pub fn check_root(root: &str) -> Result<String, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading /proc/self/mountinfo: {e}"))?;
    check_root_in(root, &mountinfo)
}

fn check_root_in(root: &str, mountinfo: &str) -> Result<String, String> {
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
fn mounts(mountinfo: &str) -> impl Iterator<Item = (PathBuf, String)> + '_ {
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
pub async fn serve(root: &str, mut recv: RecvStream, mut send: SendStream) -> Result<()> {
    let store = format!("local?root={root}");
    let mut daemon = tokio::process::Command::new("nix")
        // `--force-trusted` needs `daemon-trust-override`.
        .args([
            "--extra-experimental-features",
            "nix-command daemon-trust-override",
            "--store",
            &store,
        ])
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
        let _ = send.finish();
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

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:21 / / rw,relatime - tmpfs tmpfs rw,mode=755
23 22 7:0 / /nix/.ro-store ro,relatime - squashfs /dev/loop0 ro
24 22 0:22 / /nix/store rw,relatime - overlay overlay rw,lowerdir=/nix/.ro-store
30 22 253:1 / /mnt rw,relatime - ext4 /dev/vdb1 rw
31 30 253:2 / /mnt/boot rw,relatime - vfat /dev/vdb2 rw
40 22 0:40 / /ramdisk rw - tmpfs tmpfs rw
50 22 253:3 / /zroot rw - zfs zroot rw
51 50 253:4 / /zroot/nix rw - zfs zroot/nix rw
60 22 253:5 / /with\\040space rw - xfs /dev/vdc1 rw
";

    #[test]
    fn accepts_disk_mounts() {
        assert_eq!(check_root_in("/mnt", MOUNTINFO), Ok("ext4".into()));
        // A separate dataset for /nix is what counts.
        assert_eq!(check_root_in("/zroot", MOUNTINFO), Ok("zfs".into()));
        assert_eq!(check_root_in("/with space", MOUNTINFO), Ok("xfs".into()));
    }

    #[test]
    fn refuses_ram_and_unmounted_roots() {
        let err = |root| check_root_in(root, MOUNTINFO).unwrap_err();
        assert!(err("/ramdisk").contains("tmpfs"), "{}", err("/ramdisk"));
        assert!(
            err("/srv").contains("nothing is mounted at /srv"),
            "{}",
            err("/srv")
        );
        assert!(err("/").contains("live system"), "{}", err("/"));
        assert!(err("mnt").contains("absolute"), "{}", err("mnt"));
        assert!(
            err("/mnt/../srv").contains("normalised"),
            "{}",
            err("/mnt/../srv")
        );
    }

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(unescape("/a\\040b\\011c"), "/a b\tc");
        assert_eq!(unescape("/plain"), "/plain");
        assert_eq!(unescape("/trailing\\04"), "/trailing\\04");
        // Only real octal escapes: not signs, not 8 or 9, nothing above \377.
        assert_eq!(unescape("/x\\+12"), "/x\\+12");
        assert_eq!(unescape("/x\\089"), "/x\\089");
        assert_eq!(unescape("/x\\400"), "/x\\400");
        assert_eq!(unescape("/x\\377"), "/x\u{fffd}");
    }
}
