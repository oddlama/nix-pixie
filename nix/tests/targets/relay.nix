# The ext4 layout on `checks.vm-relay`'s second network: the installed system
# takes over the target's address there, and reaches the controller's network
# through the machine in the middle.
{ lib, ... }:
{
  imports = [ ./ext4.nix ];

  networking.interfaces.eth1.ipv4 = {
    addresses = lib.mkForce [
      {
        address = "192.168.2.4";
        prefixLength = 24;
      }
    ];
    routes = [
      {
        address = "192.168.1.0";
        prefixLength = 24;
        via = "192.168.2.3";
      }
    ];
  };
}
