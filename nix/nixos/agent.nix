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
  exe = lib.getExe cfg.package;
  # `pixie relay URL` and friends, typed on the target's console. They talk
  # to the agent's control socket, which only root may use.
  pixie = pkgs.writeShellScriptBin "pixie" ''
    if [ "$(id -u)" != 0 ] && command -v sudo >/dev/null; then
      exec sudo ${exe} "$@"
    fi
    exec ${exe} "$@"
  '';
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

    console.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether tty1 shows the agent's code, relay and messages, with a
        prompt for commands like `relay URL` (`nix-pixie console`), instead
        of a login prompt. Kernel messages go to tty12 then, and the agent's
        output only to the journal, so nothing scribbles over it.
      '';
    };

    statusTty = {
      device = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "ttyGS0";
        description = ''
          A serial terminal (a USB gadget's, a UART) where the agent's status
          goes line by line: its code, its relay, its messages. Commands
          such as `relay URL` come back the same way. For whoever, or
          whatever, is on the other end of a device without a screen.
        '';
      };
      json = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "JSON lines (`nix-pixie console --json`) rather than text, for a program to read.";
      };
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
        # Kept until the next boot: a restarted agent must still know which
        # disks it formatted, or their mounts would keep it from formatting
        # them again.
        RuntimeDirectoryPreserve = true;
        # The pairing code is printed on stdout: show it on the console too,
        # unless tty1 shows it already.
        StandardOutput = if cfg.console.enable then "journal" else "journal+console";
        Restart = "always";
        RestartSec = 2;
      };
    };

    environment.systemPackages = [ pixie ];

    systemd.services.nix-pixie-console = lib.mkIf cfg.console.enable {
      description = "nix-pixie on tty1";
      wantedBy = [ "multi-user.target" ];
      after = [
        "nix-pixie-agent.service"
        "systemd-user-sessions.service"
      ];
      conflicts = [ "getty@tty1.service" ];
      serviceConfig = {
        # Kernel messages to tty12, not over the screen; `-`: a console
        # that isn't a VT (a serial one) has none to redirect.
        ExecStartPre = "-${pkgs.util-linux}/bin/setterm --msgredirect 12";
        ExecStart = "${exe} console";
        StandardInput = "tty";
        StandardOutput = "tty";
        TTYPath = "/dev/tty1";
        TTYReset = true;
        TTYVHangup = true;
        TTYVTDisallocate = true;
        Restart = "always";
        RestartSec = 1;
      };
    };
    systemd.services."getty@tty1".enable = lib.mkIf cfg.console.enable false;

    systemd.services.nix-pixie-status = lib.mkIf (cfg.statusTty.device != null) (
      let
        device = "dev-${cfg.statusTty.device}.device";
      in
      {
        description = "nix-pixie on ${cfg.statusTty.device}";
        # The terminal may show up later, e.g. once a USB host connects.
        wantedBy = [ device ];
        bindsTo = [ device ];
        after = [
          device
          "nix-pixie-agent.service"
        ];
        conflicts = [ "serial-getty@${cfg.statusTty.device}.service" ];
        serviceConfig = {
          ExecStart = "${exe} console ${if cfg.statusTty.json then "--json" else "--plain"}";
          StandardInput = "tty";
          StandardOutput = "tty";
          TTYPath = "/dev/${cfg.statusTty.device}";
          TTYReset = true;
          Restart = "always";
          RestartSec = 1;
        };
      }
    );
    systemd.services."autovt@tty1".enable = lib.mkIf cfg.console.enable false;

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
