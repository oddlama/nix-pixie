# ext4 root on an mdadm RAID 1 across two disks, GRUB on both.
{ lib, ... }:
{
  imports = [ ./base.nix ];

  disko.devices = {
    disk = lib.genAttrs [ "a" "b" ] (_: {
      type = "disk";
      content = {
        type = "gpt";
        partitions = {
          bios = {
            size = "1M";
            type = "EF02";
          };
          mdadm = {
            size = "100%";
            content = {
              type = "mdraid";
              name = "root";
            };
          };
        };
      };
    });
    mdadm.root = {
      type = "mdadm";
      level = 1;
      content = {
        type = "filesystem";
        format = "ext4";
        mountpoint = "/";
      };
    };
  };
}
