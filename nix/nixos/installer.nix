# What the nix-pixie installer adds to a NixOS live system: the agent, what
# disko needs for any storage stack, and networking that works on any wired
# link, with or without a DHCP server.
#
# Used by the installer ISO, and importable into custom images:
#   nixosSystem { modules = [ "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix" nix-pixie.nixosModules.installer ]; }
{
  config,
  lib,
  pkgs,
  ...
}:
let
  # Router advertisements on one link (`nix-pixie-advertise@<link>`): not as
  # a router (lifetime 0) and without prefixes, so nobody routes through this
  # machine or gets an address from it. All they do is complete IPv6 setup on
  # the other end.
  advertise = pkgs.writeShellApplication {
    name = "nix-pixie-advertise";
    runtimeInputs = [ pkgs.radvd ];
    text = ''
      link=$1
      cat > "$RUNTIME_DIRECTORY/radvd.conf" <<EOF
      interface $link {
        AdvSendAdvert on;
        AdvDefaultLifetime 0;
        MinRtrAdvInterval 10;
        MaxRtrAdvInterval 30;
      };
      EOF
      exec radvd --nodaemon --logmethod stderr_clean \
        --config "$RUNTIME_DIRECTORY/radvd.conf" --pidfile "$RUNTIME_DIRECTORY/radvd.pid"
    '';
  };

  # Starts the advertisements on every wired link that has been up for a
  # while without anybody else on it giving addresses (DHCP) or advertising a
  # router: a direct cable. Stops them when the cable is pulled, so the next
  # network gets looked at anew.
  lonelyLinks = pkgs.writeShellApplication {
    name = "nix-pixie-lonely-links";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.iproute2
      config.systemd.package
    ];
    text = ''
      declare -A since
      while true; do
        for dev in /sys/class/net/*; do
          link=''${dev##*/}
          # Wired, physical Ethernet: ARPHRD_ETHER (as Wi-Fi is too), with a
          # device behind it.
          if [ ! -e "$dev/device" ] || [ -e "$dev/wireless" ] || [ "$(cat "$dev/type")" != 1 ]; then
            continue
          fi
          unit="nix-pixie-advertise@$link.service"
          if [ "$(cat "$dev/carrier" 2>/dev/null || echo 0)" != 1 ]; then
            unset "since[$link]"
            if systemctl is-active -q "$unit"; then systemctl stop "$unit"; fi
            continue
          fi
          : "''${since[$link]:=$SECONDS}"
          if (( SECONDS - since[$link] < 10 )) || systemctl is-active -q "$unit"; then
            continue
          fi
          if [ -n "$(ip -4 address show dev "$link" scope global)" ] \
            || [ -n "$(ip -6 route show dev "$link" proto ra)" ]; then
            continue
          fi
          echo "$link: no DHCP server and no router there; advertising, so the other end keeps the link up"
          systemctl start "$unit"
        done
        sleep 5
      done
    '';
  };
in
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

  # A direct cable to a machine with NetworkManager, e.g. the operator's
  # laptop: its default profile waits for DHCP, gives up after 45 s, and
  # drops its link-local address while it starts over. A connection to the
  # controller doesn't survive that. A router advertisement completes the
  # profile's IPv6 setup instead, so the link stays up (`checks.vm-link-local`).
  # Never on links where somebody else serves addresses or routes: another
  # advertisement there could change what its hosts do.
  systemd.services.nix-pixie-lonely-links = {
    description = "nix-pixie: advertise on wired links nobody else serves";
    wantedBy = [ "multi-user.target" ];
    after = [ "systemd-networkd.service" ];
    serviceConfig.ExecStart = lib.getExe lonelyLinks;
  };
  systemd.services."nix-pixie-advertise@" = {
    description = "nix-pixie: router advertisements (as no router) on %i";
    serviceConfig = {
      ExecStart = "${lib.getExe advertise} %i";
      RuntimeDirectory = "nix-pixie-advertise/%i";
    };
  };
}
