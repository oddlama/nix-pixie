# What every installed test system shares: small, bootable in QEMU, driven
# by the test driver, reachable over SSH where the installer was. The
# layouts next to this file import it and add disko devices.
#
# Evaluated both by the test and inside the controller VM (see
# `../user-flake.nix`), so nothing here may depend on where this file lives.
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

  # The target's address on the test network: nodes are numbered
  # alphabetically, and the install tests name theirs controller, installed
  # and target. So the controller finds the installed system where it found
  # the installer.
  networking.usePredictableInterfaceNames = false;
  networking.useDHCP = false;
  networking.interfaces.eth1.ipv4.addresses = [
    {
      address = "192.168.1.3";
      prefixLength = 24;
    }
  ];

  services.openssh.enable = true;
}
