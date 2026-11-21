# The system `checks.vm-install` installs and then boots, minus disk "main"'s
# device. Evaluated both by the test and inside the controller VM (see
# `user-flake.nix`), so nothing here may depend on where this file lives.
{ modulesPath, ... }:
{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    # Lets the test driver run commands on the installed system.
    (modulesPath + "/testing/test-instrumentation.nix")
  ];

  system.stateVersion = "26.11";
  networking.hostName = "web1";

  # The target's address on the test network (node 3 of 3, see
  # `install.nix`), so the controller finds the installed system where it
  # found the installer.
  networking.usePredictableInterfaceNames = false;
  networking.useDHCP = false;
  networking.interfaces.eth1.ipv4.addresses = [
    {
      address = "192.168.1.3";
      prefixLength = 24;
    }
  ];

  services.openssh.enable = true;

  # BIOS GRUB on the disk's boot partition.
  disko.devices.disk.main = {
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        boot = {
          size = "1M";
          type = "EF02";
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
