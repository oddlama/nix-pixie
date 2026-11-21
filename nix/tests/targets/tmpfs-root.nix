# Impermanence-style: / is a tmpfs, only /boot and /nix are on disk, and
# the SSH host key lives under /nix, where `services.openssh.hostKeys` says.
{
  imports = [ ./base.nix ];

  services.openssh.hostKeys = [
    {
      type = "ed25519";
      path = "/nix/persist/ssh_host_ed25519_key";
    }
  ];

  disko.devices = {
    nodev."/" = {
      fsType = "tmpfs";
      mountOptions = [
        "size=256M"
        "mode=755"
      ];
    };
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
          nix = {
            size = "100%";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/nix";
            };
          };
        };
      };
    };
  };
}
