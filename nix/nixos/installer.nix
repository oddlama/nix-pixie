# What the nix-pixie installer adds to a NixOS live system: the agent, what
# disko needs for any storage stack, and networking that works on any wired
# link, with or without a DHCP server.
#
# Used by the installer ISO, and importable into custom images:
#   nixosSystem { modules = [ "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix" nix-pixie.nixosModules.installer ]; }
{ lib, ... }:
{
  imports = [
    ./agent.nix
    ./storage.nix
  ];

  services.nix-pixie-agent.enable = true;

  # Wired links belong to networkd. The NixOS installer profile also enables
  # NetworkManager (for `nmtui` and Wi-Fi), which would otherwise manage them
  # too. Its default profile gives up on DHCP every 45 s and drops the
  # link-local address while it retries (see nix/tests/link-local.nix).
  networking.useNetworkd = true;
  networking.useDHCP = lib.mkForce false;
  networking.networkmanager.unmanaged = [ "type:ethernet" ];
  systemd.network.networks."40-nix-pixie-wired" = {
    matchConfig.Type = "ether";
    networkConfig = {
      DHCP = "yes";
      # Direct cable, or no DHCP server: IPv4 (169.254/16) and IPv6 (fe80::/64)
      # link-local addresses still come up, and that's all discovery needs.
      LinkLocalAddressing = "yes";
      IPv6AcceptRA = true;
    };
    # Don't hold up boot waiting for a lease that may never come.
    linkConfig.RequiredForOnline = "no";
  };
}
