//! Checks that fail before anything is erased, when the configuration can't
//! work on the target as it is.

use anyhow::{Result, bail};
use nix_pixie_proto::inventory::Firmware;

use crate::metadata::BootCheck;
use crate::system::{Boot, Ssh};

/// Whether checking the installed system by its SSH host key can be fooled:
/// a first boot that reboots to enroll the Secure Boot keys (lanzaboote's
/// `autoReboot`) would answer with the key, and the check report it up,
/// unless the SSH server waits for the keys.
pub fn check_boot_check(
    boot: &Boot,
    ssh: &Ssh,
    boot_check: BootCheck,
    configuration: &str,
) -> Result<()> {
    if boot.enroll_keys
        && boot.enroll_reboots
        && ssh.enable
        && boot_check == BootCheck::Ssh
        && !ssh.after_key_preparation
    {
        bail!(
            "nixosConfigurations.{configuration} reboots on its first boot to enroll its Secure \
             Boot keys (boot.lanzaboote.autoEnrollKeys.autoReboot), and its SSH server may \
             answer before that: the check after the install would report the machine up \
             before Secure Boot is. Import nix-pixie.nixosModules.default, which has the SSH \
             server wait for the keys, or order sshd after prepare-sb-auto-enroll.service \
             yourself; or check another way (bootCheck = \"hooks\" or \"none\")"
        );
    }
    Ok(())
}

/// Whether the configuration's boot loader can boot on the target's
/// firmware, judged by how the target booted the installer.
pub fn check_boot(boot: &Boot, firmware: &Firmware, configuration: &str) -> Result<()> {
    let who = format!("nixosConfigurations.{configuration}");
    if firmware.uefi {
        if boot.grub && boot.grub_bios && !boot.grub_efi && !boot.systemd_boot && !boot.lanzaboote {
            bail!(
                "{who} installs GRUB for BIOS only, but the target booted the installer with UEFI: \
                 it would only boot with legacy boot (CSM) enabled. Boot the installer in legacy \
                 mode if that is what you want, or use an EFI boot loader (systemd-boot, or \
                 boot.loader.grub.efiSupport)"
            );
        }
        if boot.enroll_keys && firmware.setup_mode != Some(true) {
            let state = if firmware.setup_mode.is_none() {
                "doesn't say whether it is"
            } else {
                "is not"
            };
            bail!(
                "{who} enrolls its Secure Boot keys on its first boot \
                 (boot.lanzaboote.autoEnrollKeys), which works only while the firmware is in \
                 Setup Mode. The target's firmware {state}, so the machine would run without \
                 Secure Boot. Clear its Secure Boot keys in the firmware settings (\"Reset to \
                 Setup Mode\", \"Delete all keys\" or similar), then boot the installer again"
            );
        }
        return Ok(());
    }
    let bios = "the target booted the installer in BIOS (legacy) mode. If it has UEFI, boot \
                the installer with UEFI; otherwise use GRUB for BIOS (a 1M partition of type \
                EF02)";
    if boot.systemd_boot {
        bail!("{who} uses systemd-boot, which needs UEFI, but {bios}");
    }
    if boot.lanzaboote {
        bail!("{who} uses lanzaboote, which needs UEFI, but {bios}");
    }
    if boot.grub && boot.grub_efi && !boot.grub_bios {
        bail!("{who} installs GRUB for EFI only, but {bios}");
    }
    if boot.efi_variables {
        bail!("{who} sets boot.loader.efi.canTouchEfiVariables, which needs UEFI, but {bios}");
    }
    Ok(())
}
