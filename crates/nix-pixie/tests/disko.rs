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
use nix_pixie_proto::inventory::Disk;

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

#[test]
fn refuses_disks_that_are_gone_or_in_use_right_now() {
    let disk = |name: &str| Disk {
        name: name.into(),
        size_bytes: 1 << 30,
        ..Disk::default()
    };
    let present = vec![
        Disk {
            mounts: vec!["/iso".into()],
            ..disk("vda")
        },
        disk("vdb"),
        Disk {
            mounts: vec!["/mnt".into()],
            formatted_this_boot: true,
            ..disk("vdc")
        },
        Disk {
            read_only: true,
            ..disk("sr0")
        },
    ];
    assert_eq!(
        check_present(&[assignment("main", "vdb")], &present),
        Ok(())
    );
    // Mounted by an earlier attempt during this boot.
    assert_eq!(
        check_present(&[assignment("main", "vdc")], &present),
        Ok(())
    );
    for (device, why) in [("vda", "in use"), ("sr0", "read-only"), ("vdz", "gone")] {
        let err = check_present(&[assignment("main", device)], &present).unwrap_err();
        assert!(err.contains(why), "{device}: {err}");
    }
}

#[test]
fn kept_key_files_are_overwritten_and_removed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("nix-pixie-test-wipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("secret.key");
    std::fs::write(&key, b"secretsecret").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
    // Its contents, seen through another name once it is gone.
    let other = dir.join("other-name");
    std::fs::hard_link(&key, &other).unwrap();
    drop(nix_pixie::disko::KeyFiles::new(vec![key.clone()]));
    assert!(!key.exists());
    assert_eq!(std::fs::read(&other).unwrap(), vec![0; 12]);
    std::fs::remove_dir_all(dir).unwrap();
}
