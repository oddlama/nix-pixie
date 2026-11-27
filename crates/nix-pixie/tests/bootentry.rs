//! Tests for `nix_pixie::bootentry`: reading the firmware's boot entries,
//! and when the installed system needs one of its own.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::Path;

use nix_pixie::bootentry::{Plan, mounted_device, order_with_first, parse};
use nix_pixie::system::Boot;

/// efibootmgr 18 on OVMF, after an install made its entry.
const LISTING: &str = "BootCurrent: 0001\n\
Timeout: 0 seconds\n\
BootOrder: 0001,0000,0003\n\
Boot0000* UiApp\tFvVol(7cb8bdc9-f8eb-4f34-aaea-3ee4af6516a1)/FvFile(462caa21-7614-4503-836e-8ab6f4662331)\n\
Boot0001* UEFI QEMU DVD-ROM QM00003 \tPciRoot(0x0)/Pci(0x1f,0x2)/Sata(1,65535,0){auto_created_boot_option}\n\
Boot0003* Linux Boot Manager\tHD(1,GPT,0d7ce2d5-1b0f-4b8e-9cfd-37c3bf4b1b5e,0x800,0x100000)/\\EFI\\systemd\\systemd-bootx64.efi\n\
      dp: 04 01 2a 00 01 00 00 00\n";

const PARTUUID: &str = "0D7CE2D5-1B0F-4B8E-9CFD-37C3BF4B1B5E";
const LOADER: &str = "\\EFI\\systemd\\systemd-bootx64.efi";

#[test]
fn entries_and_order_are_read() {
    let listing = parse(LISTING);
    assert_eq!(listing.order, ["0001", "0000", "0003"]);
    let numbers: Vec<_> = listing.entries.iter().map(|e| e.number.as_str()).collect();
    assert_eq!(numbers, ["0000", "0001", "0003"]);
    // Ours, by partition and file, whatever the case.
    let ours: Vec<_> = listing
        .entries
        .iter()
        .filter(|e| e.boots(PARTUUID, LOADER))
        .map(|e| e.number.as_str())
        .collect();
    assert_eq!(ours, ["0003"]);
    assert!(!listing.entries[2].boots("11111111-2222-3333-4444-555555555555", LOADER));
    assert!(!listing.entries[2].boots(PARTUUID, "\\EFI\\BOOT\\BOOTX64.EFI"));
}

#[test]
fn older_efibootmgr_wraps_the_file_path() {
    let old = "BootOrder: 0002\nBoot0002* Linux Boot Manager\tHD(1,GPT,0d7ce2d5-1b0f-4b8e-9cfd-37c3bf4b1b5e,0x800,0x100000)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n";
    assert!(parse(old).entries[0].boots(PARTUUID, LOADER));
}

#[test]
fn the_entry_goes_first_once() {
    let order: Vec<String> = ["0001", "0000", "0003"].map(String::from).to_vec();
    assert_eq!(order_with_first(&order, "0003"), ["0003", "0001", "0000"]);
    assert_eq!(order_with_first(&order, "0001"), ["0001", "0000", "0003"]);
    assert_eq!(order_with_first(&[], "0004"), ["0004"]);
}

#[test]
fn the_partition_mounted_last_at_a_path_is_found() {
    let mountinfo = "\
22 1 0:21 / / rw,relatime - tmpfs tmpfs rw\n\
41 22 254:2 / /mnt rw,relatime - ext4 /dev/vdb2 rw\n\
42 41 254:1 / /mnt/boot rw,relatime - vfat /dev/vdb1 rw,umask=0077\n\
43 41 254:5 / /mnt/my\\040boot rw - vfat /dev/vdc1 rw\n\
44 41 254:9 / /mnt/boot rw - vfat /dev/vdd1 rw\n";
    assert_eq!(
        mounted_device(mountinfo, Path::new("/mnt/boot")).as_deref(),
        Some("254:9")
    );
    assert_eq!(
        mounted_device(mountinfo, Path::new("/mnt/my boot")).as_deref(),
        Some("254:5")
    );
    assert_eq!(mounted_device(mountinfo, Path::new("/mnt/efi")), None);
}

#[test]
fn only_lanzaboote_on_uefi_needs_an_entry_made() {
    let lanzaboote = Boot {
        lanzaboote: true,
        efi_variables: true,
        esp_mount_point: "/boot".into(),
        ..Boot::default()
    };
    assert_eq!(
        Plan::of(&lanzaboote, true, "/mnt"),
        Plan::Add {
            esp: "/mnt/boot".into()
        }
    );
    let elsewhere = Boot {
        esp_mount_point: "/efi".into(),
        ..lanzaboote.clone()
    };
    assert_eq!(
        Plan::of(&elsewhere, true, "/target"),
        Plan::Add {
            esp: "/target/efi".into()
        }
    );
    let forbidden = Boot {
        efi_variables: false,
        ..lanzaboote.clone()
    };
    assert_eq!(Plan::of(&forbidden, true, "/mnt"), Plan::NotAllowed);
    // BIOS, or a boot loader that makes its own (systemd-boot, GRUB).
    assert_eq!(Plan::of(&lanzaboote, false, "/mnt"), Plan::Nothing);
    let systemd_boot = Boot {
        systemd_boot: true,
        efi_variables: true,
        ..Boot::default()
    };
    assert_eq!(Plan::of(&systemd_boot, true, "/mnt"), Plan::Nothing);
}
