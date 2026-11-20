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
      description = "Open the agent's port, its probe port (UDP 47012) and mDNS (UDP 5353).";
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
      path = [
        pkgs.util-linux
        pkgs.nixos-facter
        config.nix.package
      ];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} agent --port ${toString cfg.port} --state-dir /run/nix-pixie";
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
