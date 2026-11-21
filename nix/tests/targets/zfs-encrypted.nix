# ZFS root with native encryption, like disko's zfs-encrypted-root example:
# created with the key from a file (nix-pixie asks the operator and puts it
# there), then switched to prompting, so the installed system asks for it on
# the console at boot.
{
  imports = [ ./base.nix ];

  networking.hostId = "8425e349";
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
      rootFsOptions.mountpoint = "none";
      datasets = {
        root = {
          type = "zfs_fs";
          mountpoint = "/";
          options = {
            mountpoint = "legacy";
            encryption = "aes-256-gcm";
            keyformat = "passphrase";
            keylocation = "file:///tmp/secret.key";
          };
          postCreateHook = "zfs set keylocation=prompt zroot/root";
        };
        "root/nix" = {
          type = "zfs_fs";
          mountpoint = "/nix";
          options.mountpoint = "legacy";
        };
      };
    };
  };
}
