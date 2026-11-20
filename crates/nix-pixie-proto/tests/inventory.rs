//! Tests for `nix_pixie_proto::inventory`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::inventory::*;

#[test]
fn disk_ids() {
    for ok in [
        "nvme-Samsung_SSD_980_1TB_S64ANS0T123456",
        "virtio-pixie-data",
        "wwn-0x5000c500a1b2c3d4",
        "ata-WDC_WD40EFRX-68N32N0_WD-WCC7K1234567",
    ] {
        assert!(is_valid_disk_id(ok), "{ok}");
    }
    for bad in [
        "",
        ".",
        "..",
        "a/b",
        "a b",
        "x;rm -rf /",
        "$(id)",
        "a\nb",
        "ä",
    ] {
        assert!(!is_valid_disk_id(bad), "{bad:?}");
    }
}

#[test]
fn has_data() {
    let empty = Disk::default();
    assert!(!empty.has_data());
    let gpt = Disk {
        partition_table: Some("gpt".into()),
        ..Disk::default()
    };
    assert!(gpt.has_data());
    let fs = Disk {
        contents: vec![Content::default()],
        ..Disk::default()
    };
    assert!(fs.has_data());
}

#[test]
fn systems() {
    assert_eq!(nix_system("x86_64"), "x86_64-linux");
    assert_eq!(nix_system("aarch64"), "aarch64-linux");
    assert_eq!(nix_system("riscv64"), "riscv64-linux");
}
