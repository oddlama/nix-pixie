//! Tests for `nix_pixie::preflight`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::preflight::check_boot;
use nix_pixie::system::Boot;
use nix_pixie_proto::inventory::Firmware;

const BIOS: Firmware = Firmware {
    uefi: false,
    secure_boot: None,
    setup_mode: None,
};
const UEFI: Firmware = Firmware {
    uefi: true,
    secure_boot: Some(false),
    setup_mode: Some(false),
};

fn grub(efi: bool, bios: bool) -> Boot {
    Boot {
        grub: true,
        grub_efi: efi,
        grub_bios: bios,
        ..Boot::default()
    }
}

fn systemd_boot() -> Boot {
    Boot {
        systemd_boot: true,
        efi_variables: true,
        ..Boot::default()
    }
}

fn refusal(boot: &Boot, firmware: &Firmware) -> String {
    format!("{:#}", check_boot(boot, firmware, "web1").unwrap_err())
}

#[test]
fn loaders_that_fit_the_firmware_pass() {
    for (boot, firmware) in [
        (grub(false, true), &BIOS),
        (grub(true, true), &BIOS),
        (grub(true, true), &UEFI),
        (grub(true, false), &UEFI),
        (systemd_boot(), &UEFI),
        // No boot loader of ours: nothing to check.
        (Boot::default(), &BIOS),
        (Boot::default(), &UEFI),
    ] {
        assert!(
            check_boot(&boot, firmware, "web1").is_ok(),
            "{boot:?} on {firmware:?}"
        );
    }
}

#[test]
fn efi_loaders_are_refused_on_bios() {
    let sd = refusal(&systemd_boot(), &BIOS);
    assert!(
        sd.contains("systemd-boot") && sd.contains("BIOS (legacy) mode"),
        "{sd}"
    );
    let lanzaboote = Boot {
        lanzaboote: true,
        ..Boot::default()
    };
    assert!(refusal(&lanzaboote, &BIOS).contains("lanzaboote"));
    assert!(refusal(&grub(true, false), &BIOS).contains("GRUB for EFI only"));
    let variables = Boot {
        efi_variables: true,
        ..grub(false, true)
    };
    assert!(refusal(&variables, &BIOS).contains("canTouchEfiVariables"));
}

#[test]
fn bios_only_grub_is_refused_on_uefi() {
    let err = refusal(&grub(false, true), &UEFI);
    assert!(err.contains("BIOS only") && err.contains("CSM"), "{err}");
}
