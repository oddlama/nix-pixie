# ZFS root with a dataset for /nix, /boot on ext4 (GRUB can't read every
# ZFS feature, zstd compression among them).
{
  imports = [ ./base.nix ];

  networking.hostId = "8425e349";
  # The installer exports the pool, so the first boot needs no forced
  # import. Forbid it, so the test notices if the export goes missing.
  boot.zfs.forceImportRoot = false;

  disko.devices = {
    disk.main = {
      type = "disk";
      content = {
        type = "gpt";
        partitions = {
          bios = {
            size = "1M";
            type = "EF02";
          };
          boot = {
            size = "256M";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/boot";
            };
          };
          zfs = {
            size = "100%";
            content = {
              type = "zfs";
              pool = "zroot";
            };
          };
        };
      };
    };
    zpool.zroot = {
      type = "zpool";
      rootFsOptions = {
        mountpoint = "none";
        compression = "zstd";
      };
      datasets = {
        root = {
          type = "zfs_fs";
          mountpoint = "/";
          options.mountpoint = "legacy";
        };
        nix = {
          type = "zfs_fs";
          mountpoint = "/nix";
          options.mountpoint = "legacy";
        };
      };
    };
  };
}
