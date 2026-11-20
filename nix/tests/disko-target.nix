# The configuration `checks.vm-disko` installs, minus the disk's device (set
# per machine). Evaluated twice: by the test, to prebuild everything into the
# controller VM's store, and by the controller inside the VM, from the test
# flake. Both must yield the same derivations, so nothing here may depend on
# where this file lives.
{ modulesPath, ... }:
{
  imports = [ (modulesPath + "/profiles/minimal.nix") ];

  system.stateVersion = "26.11";

  # BIOS GRUB with a boot partition (EF02): the layout that keeps using the
  # disk's path after the install.
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
