//! Tests for `nix_pixie::store`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::store::*;

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
