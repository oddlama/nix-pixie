# systemd-boot on an EFI system partition, ext4 root. Installed from the
# installer ISO booted with UEFI (`checks.vm-iso-uefi`), which writes its
# boot entry into the firmware's variables.
{ lib, ... }:
{
  imports = [ ./base.nix ];

  boot.loader = {
    grub.enable = false;
    systemd-boot.enable = true;
    efi.canTouchEfiVariables = true;
  };

  # Found by its link-local address, like the installer before it: the ISO
  # test has no other address for the target.
  networking.interfaces.eth1.ipv4.addresses = lib.mkForce [ ];

  disko.devices.disk.main = {
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        esp = {
          size = "512M";
          type = "EF00";
          content = {
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
            mountOptions = [ "umask=0077" ];
          };
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };
}
