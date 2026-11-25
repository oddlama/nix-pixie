# The installer-side agent as a systemd service. Used by the installer image
# and the VM tests.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nix-pixie-agent;
  relayFlags =
    if !cfg.relay.enable then
      [ "--no-relay" ]
    else
      lib.concatMap (url: [
        "--relay"
        url
      ]) cfg.relay.urls;
in
{
  options.services.nix-pixie-agent = {
    enable = lib.mkEnableOption "the nix-pixie installer agent";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The nix-pixie package.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 47011;
      description = "UDP port of the agent's iroh endpoint.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Open the agent's port, its probe port (UDP 47012) and mDNS (UDP 5353).
        Controllers on the same network find the agent through these; one
        that finds it at a relay needs none of them.
      '';
    };

    relay = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Whether the agent waits at a relay server while it shows a code, so
          a controller on another network finds it without this machine
          being reachable from there. The relay also carries their
          (end-to-end encrypted) traffic until they have a direct path.

          Without it, pairing codes are shorter (`42-acid-acorn-acre`) and
          only controllers on the same network, or with `--target`, find
          this machine.
        '';
      };
      urls = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "https://relay.example.org" ];
        description = ''
          The relay servers (`iroh-relay`) to use, instead of the public ones
          run by n0, the makers of iroh. The controller needs the same:
          `nix-pixie controller install --relay URL`.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.nix-pixie-agent = {
      description = "nix-pixie installer agent";
      wantedBy = [ "multi-user.target" ];
      # Deliberately not network-online.target: an airgapped installer may
      # never get a DHCP lease. mDNS picks up interfaces as they come up.
      after = [ "network.target" ];
      # Inventory: lsblk for disks, nixos-facter for the hardware report.
      # Store push: `nix daemon --stdio` serving the store under /mnt.
      # Formatting: disko's scripts call modprobe (dm-snapshot for LVM,
      # md_mod for mdadm) and expect it on the PATH.
      # Install: ssh-keygen for pulled host keys, nixos-install, and zpool
      # to export pools before the first boot. Hooks' programs in the
      # installed system: nixos-enter.
      path = [
        pkgs.util-linux
        pkgs.nixos-facter
        config.nix.package
        pkgs.kmod
        pkgs.openssh
        pkgs.nixos-install
        pkgs.nixos-enter
      ]
      ++ lib.optional (config.boot.supportedFilesystems.zfs or false) config.boot.zfs.package;
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe cfg.package)
            "agent"
            "--port"
            (toString cfg.port)
            "--state-dir"
            "/run/nix-pixie"
          ]
          ++ relayFlags
        );
        RuntimeDirectory = "nix-pixie";
        RuntimeDirectoryMode = "0700";
        # The pairing code is printed on stdout: show it on the console too.
        StandardOutput = "journal+console";
        Restart = "always";
        RestartSec = 2;
      };
    };

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedUDPPorts = [
        cfg.port
        # Discovery probes; fixed in the protocol (announce::PROBE_PORT).
        47012
        5353
      ];
    };
  };
}
