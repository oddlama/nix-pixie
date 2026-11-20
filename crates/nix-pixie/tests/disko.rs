//! Tests for `nix_pixie::disko`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::PathBuf;

use nix_pixie::disko::*;
use nix_pixie_proto::ctrl::DiskAssignment;

fn assignment(role: &str, device: &str) -> DiskAssignment {
    DiskAssignment {
        role: role.into(),
        device: device.into(),
        link: true,
    }
}

#[test]
fn plain_names() {
    for ok in ["main", "data0", "nvme0n1", "mmcblk0", "a.b_c-d"] {
        assert!(is_plain_name(ok), "{ok}");
    }
    for bad in [
        "",
        "-x",
        ".hidden",
        "a/b",
        "..",
        "a b",
        "x\n",
        &"a".repeat(65),
    ] {
        assert!(!is_plain_name(bad), "{bad:?}");
    }
}

#[test]
fn assignments_must_be_unique_and_plain() {
    assert_eq!(
        check_assignments(&[assignment("main", "vdb"), assignment("data", "vdc")]),
        Ok(())
    );
    assert!(check_assignments(&[]).is_err());
    let twice_role = check_assignments(&[assignment("main", "vdb"), assignment("main", "vdc")]);
    assert!(twice_role.unwrap_err().contains("role main"));
    let twice_disk = check_assignments(&[assignment("main", "vdb"), assignment("data", "vdb")]);
    assert!(twice_disk.unwrap_err().contains("vdb"));
    assert!(check_assignments(&[assignment("../etc", "vdb")]).is_err());
    assert!(check_assignments(&[assignment("main", "../sda")]).is_err());
}

#[test]
fn links_like_udev_and_replaces_atomically() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("nix-pixie-test-links-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let by_pixie = dir.join("disk/by-pixie");

    link(&by_pixie, "main", "vdb").unwrap();
    assert_eq!(
        std::fs::read_link(by_pixie.join("main")).unwrap(),
        PathBuf::from("../../vdb")
    );
    // A second install with another disk replaces the link.
    link(&by_pixie, "main", "vdc").unwrap();
    assert_eq!(
        std::fs::read_link(by_pixie.join("main")).unwrap(),
        PathBuf::from("../../vdc")
    );
    let names: Vec<_> = std::fs::read_dir(&by_pixie)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["main"], "no temporary files left");
    std::fs::remove_dir_all(&dir).unwrap();
}
