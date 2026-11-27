//! Tests for `nix_pixie::preflight`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::metadata::BootCheck;
use nix_pixie::preflight::{check_boot, check_boot_check};
use nix_pixie::system::{Boot, Ssh};
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

fn enrolling(setup_mode: Option<bool>) -> (Boot, Firmware) {
    let boot = Boot {
        lanzaboote: true,
        enroll_keys: true,
        ..Boot::default()
    };
    (boot, Firmware { setup_mode, ..UEFI })
}

#[test]
fn enrolling_secure_boot_keys_needs_setup_mode() {
    let (boot, firmware) = enrolling(Some(true));
    assert!(check_boot(&boot, &firmware, "web1").is_ok());
    let (boot, firmware) = enrolling(Some(false));
    let err = refusal(&boot, &firmware);
    assert!(
        err.contains("is not") && err.contains("Setup Mode") && err.contains("autoEnrollKeys"),
        "{err}"
    );
    let (boot, firmware) = enrolling(None);
    assert!(refusal(&boot, &firmware).contains("doesn't say whether it is"));
}

#[test]
fn lanzaboote_without_enrolling_leaves_the_keys_alone() {
    let boot = Boot {
        lanzaboote: true,
        ..Boot::default()
    };
    // Keys already enrolled, by hand or by an earlier install.
    assert!(check_boot(&boot, &UEFI, "web1").is_ok());
}

#[test]
fn bios_only_grub_is_refused_on_uefi() {
    let err = refusal(&grub(false, true), &UEFI);
    assert!(err.contains("BIOS only") && err.contains("CSM"), "{err}");
}

fn ssh(after_key_preparation: bool) -> Ssh {
    Ssh {
        enable: true,
        ports: vec![22],
        host_key_path: None,
        after_key_preparation,
    }
}

#[test]
fn a_first_boot_that_reboots_to_enroll_must_not_answer_ssh() {
    let reboots = Boot {
        lanzaboote: true,
        enroll_keys: true,
        enroll_reboots: true,
        ..Boot::default()
    };
    let err = format!(
        "{:#}",
        check_boot_check(&reboots, &ssh(false), BootCheck::Ssh, "web1").unwrap_err()
    );
    assert!(err.contains("nixosModules.default"), "{err}");
    assert!(err.contains("autoReboot"), "{err}");

    // The SSH server waits for the keys; or nothing looks at it; or the first
    // boot doesn't reboot (the keys are enrolled on the next, whenever).
    assert!(check_boot_check(&reboots, &ssh(true), BootCheck::Ssh, "web1").is_ok());
    assert!(check_boot_check(&reboots, &ssh(false), BootCheck::Hooks, "web1").is_ok());
    assert!(check_boot_check(&reboots, &ssh(false), BootCheck::Nothing, "web1").is_ok());
    let without_ssh = Ssh {
        enable: false,
        ..ssh(false)
    };
    assert!(check_boot_check(&reboots, &without_ssh, BootCheck::Ssh, "web1").is_ok());
    let no_reboot = Boot {
        enroll_reboots: false,
        ..reboots.clone()
    };
    assert!(check_boot_check(&no_reboot, &ssh(false), BootCheck::Ssh, "web1").is_ok());
}
