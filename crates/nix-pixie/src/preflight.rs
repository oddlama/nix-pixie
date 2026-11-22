//! Checks that fail before anything is erased, when the configuration can't
//! work on the target as it is.

use anyhow::{Result, bail};
use nix_pixie_proto::inventory::Firmware;

use crate::system::Boot;

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
