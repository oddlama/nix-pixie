# The UEFI system with lanzaboote instead of plain systemd-boot: keys created
# and enrolled on the first boot, which reboots by itself for systemd-boot to
# enroll them (`checks.vm-iso-secure-boot`).
{ lib, ... }:
{
  imports = [ ./uefi.nix ];

  boot.loader.systemd-boot.enable = lib.mkForce false;
  boot.lanzaboote = {
    enable = true;
    pkiBundle = "/var/lib/sbctl";
    autoGenerateKeys.enable = true;
    autoEnrollKeys = {
      enable = true;
      autoReboot = true;
    };
  };
}
