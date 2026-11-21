# ext4 root on LUKS, /boot unencrypted. disko reads the passphrase from
# passwordFile while formatting (nix-pixie asks the operator and puts it
# there); the installed system asks for it on the console at boot.
{
  imports = [ ./base.nix ];

  disko.devices.disk.main = {
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
        root = {
          size = "100%";
          content = {
            type = "luks";
            name = "cryptroot";
            passwordFile = "/tmp/secret.key";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
            };
          };
        };
      };
    };
  };
}
