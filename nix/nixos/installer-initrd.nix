# An installer that is nothing but a kernel and a systemd stage-1 initrd
# (`boot.initrd.systemd`): for devices that boot over USB or the network
# rather than from a medium, often without a screen. It never switches to a
# root filesystem; the agent runs in the initrd until the install reboots
# the device.
#
# Build it as a NixOS configuration of its own, for the device's platform
# and with its kernel, and use `system.build.nixPixieInstaller`: the
# kernel, the initrd and the kernel command line.
#
#   nixpkgs.lib.nixosSystem {
#     modules = [
#       nix-pixie.nixosModules.installer-initrd
#       {
#         nixpkgs.hostPlatform = "aarch64-linux";
#         boot.kernelPackages = ...;   # the device's kernel
#         boot.initrd.nix-pixie-agent.usbGadget.enable = true;
#       }
#     ];
#   }
#
# The tools the agent runs (Nix, nixos-install, util-linux, ...) go into a
# second archive, prepended to the initrd, with their whole closures and a
# Nix database registering them: the systemd initrd copies only what it is
# told to, which would break their scripts.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.boot.initrd.nix-pixie-agent;

  relayFlags =
    if !cfg.relay.enable then
      [ "--no-relay" ]
    else
      lib.concatMap (url: [
        "--relay"
        url
      ]) cfg.relay.urls;

  tools = pkgs.buildEnv {
    name = "nix-pixie-initrd-tools";
    paths = [
      cfg.package
      config.nix.package
      pkgs.nixos-install
      pkgs.nixos-enter
      pkgs.util-linuxMinimal
      pkgs.coreutils
      pkgs.bashNonInteractive
      pkgs.kmod
      pkgs.openssh
    ]
    ++ lib.optional cfg.facter pkgs.nixos-facter
    ++ lib.optional (config.boot.initrd.supportedFilesystems.zfs or false) config.boot.zfs.package
    ++ cfg.extraTools;
    pathsToLink = [
      "/bin"
      "/sbin"
    ];
  };
  registration = pkgs.closureInfo { rootPaths = [ tools ]; };
  toolsArchive = pkgs.makeInitrd {
    name = "nix-pixie-initrd-tools";
    compressor = "zstd";
    contents = [
      {
        object = tools;
        symlink = "/nix-pixie/tools";
      }
      {
        object = "${registration}/registration";
        symlink = "/nix-pixie/registration";
      }
    ];
  };
  path = "/nix-pixie/tools/bin:/bin:${config.boot.initrd.systemd.package}/bin";

  agent = lib.escapeShellArgs (
    [
      "${tools}/bin/nix-pixie"
      "agent"
      "--port"
      (toString cfg.port)
      "--state-dir"
      "/run/nix-pixie"
    ]
    ++ relayFlags
  );
in
{
  options.boot.initrd.nix-pixie-agent = {
    enable = lib.mkEnableOption "the nix-pixie agent in a systemd stage-1 initrd" // {
      default = true;
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../package.nix { controllerTools = false; };
      defaultText = lib.literalExpression "nix-pixie built for this platform, without the controller's tools";
      description = "The nix-pixie package.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 47011;
      description = "UDP port of the agent's iroh endpoint.";
    };

    controllerCode.credential = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "pixie.code";
      description = ''
        The systemd credential holding a controller's code (from
        `nix-pixie controller new-code`), for pairing without a screen. Set
        it on the kernel command line,
        `systemd.set_credential=pixie.code:acid-acorn-acre`, and run
        `nix-pixie controller install --controller-code-file FILE` with the
        same code. Without the credential the agent shows a code, as on a
        screen. `null`: always show one.
      '';
    };

    relay = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Whether the agent waits at a relay. Off by default: such an
          installer is usually on a direct link, and a controller's code
          (see `controllerCode`) uses no relays anyway.
        '';
      };
      urls = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "The relay servers to use instead of n0's.";
      };
    };

    statusTty = {
      device = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = if cfg.usbGadget.enable then "ttyGS0" else null;
        defaultText = lib.literalExpression ''if usbGadget.enable then "ttyGS0" else null'';
        description = ''
          A serial terminal where the agent's status goes line by line, and
          commands come back (`nix-pixie console --plain` or `--json`).
        '';
      };
      json = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "JSON lines rather than text, for a program to read.";
      };
    };

    usbGadget = {
      enable = lib.mkEnableOption ''
        a USB gadget on the device's USB device controller: a network
        function (CDC-NCM, `usb0` on the device) for the install, and a
        serial port (CDC-ACM, `ttyGS0`) for the status. The host sees a new
        network interface and a serial port'';
      address = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "fe80::2";
        description = ''
          A fixed IPv6 link-local address on the gadget's network interface,
          so the controller can be pointed at it:
          `--target 'fe80::2%<the host's interface>'`. Each device is on a
          link of its own, so they can all have the same one.
        '';
      };
      vendorId = lib.mkOption {
        type = lib.types.str;
        default = "0x1d6b";
        description = "The gadget's USB vendor id (default: the Linux Foundation's).";
      };
      productId = lib.mkOption {
        type = lib.types.str;
        default = "0x0104";
        description = "The gadget's USB product id (default: Multifunction Composite Gadget).";
      };
      udc = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "3550000.usb";
        description = "The USB device controller to use (in /sys/class/udc); `null`: the first one.";
      };
    };

    facter = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Include nixos-facter, for machines whose configuration wants a hardware report.";
    };

    extraTools = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "More programs on the agent's PATH.";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.initrd.systemd = {
      enable = true;
      # No root to switch to: the agent runs here until the device reboots.
      root = null;
      suppressedUnits = [
        "initrd-parse-etc.service"
        "initrd-cleanup.service"
        "initrd-switch-root.service"
        "initrd-switch-root.target"
      ];
      services.initrd-find-nixos-closure.enable = false;
      services.initrd-nixos-activation.enable = false;

      network = {
        enable = true;
        networks."40-nix-pixie" = {
          matchConfig.Type = "ether";
          networkConfig = {
            DHCP = "yes";
            LinkLocalAddressing = "yes";
            IPv6AcceptRA = true;
          };
          address = lib.optional (
            cfg.usbGadget.enable && cfg.usbGadget.address != null
          ) "${cfg.usbGadget.address}/64";
          linkConfig.RequiredForOnline = "no";
        };
      };

      contents."/etc/nix/nix.conf".text = ''
        build-users-group =
        sandbox = false
        experimental-features = nix-command
      '';

      # Registers the tools' paths, so they are valid in the store the
      # agent receives into before the disks are formatted.
      services.nix-pixie-store = {
        description = "nix-pixie: register the installer's store";
        wantedBy = [ "initrd.target" ];
        before = [ "nix-pixie-agent.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          mkdir -p /nix/var/nix
          /nix-pixie/tools/bin/nix-store --load-db < /nix-pixie/registration
        '';
      };

      services.nix-pixie-agent = {
        description = "nix-pixie installer agent";
        wantedBy = [ "initrd.target" ];
        requires = [ "nix-pixie-store.service" ];
        after = [
          "nix-pixie-store.service"
          "systemd-networkd.service"
        ];
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          Environment = [ "PATH=${path}" ];
          ImportCredential = lib.mkIf (cfg.controllerCode.credential != null) cfg.controllerCode.credential;
          RuntimeDirectory = "nix-pixie";
          RuntimeDirectoryMode = "0700";
          RuntimeDirectoryPreserve = true;
          StandardOutput = "journal+console";
          Restart = "always";
          RestartSec = 2;
        };
        # With the credential, pair with that controller's code; without
        # it, show a code.
        script =
          if cfg.controllerCode.credential == null then
            "exec ${agent}"
          else
            ''
              code="''${CREDENTIALS_DIRECTORY:-/nonexistent}/${cfg.controllerCode.credential}"
              if [ -e "$code" ]; then
                exec ${agent} --controller-code-file "$code"
              fi
              exec ${agent}
            '';
      };

      services.nix-pixie-status = lib.mkIf (cfg.statusTty.device != null) {
        description = "nix-pixie on ${cfg.statusTty.device}";
        wantedBy = [ "initrd.target" ];
        after = [
          "nix-pixie-agent.service"
          "nix-pixie-usb-gadget.service"
        ];
        # The terminal may only show up once a USB host connects: retry.
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          Environment = [ "PATH=${path}" ];
          ExecStart = "${tools}/bin/nix-pixie console ${if cfg.statusTty.json then "--json" else "--plain"}";
          StandardInput = "tty";
          StandardOutput = "tty";
          TTYPath = "/dev/${cfg.statusTty.device}";
          TTYReset = true;
          Restart = "always";
          RestartSec = 1;
        };
      };

      services.nix-pixie-usb-gadget = lib.mkIf cfg.usbGadget.enable {
        description = "nix-pixie: USB gadget (network and serial)";
        wantedBy = [ "initrd.target" ];
        before = [ "systemd-networkd.service" ];
        after = [ "sys-kernel-config.mount" ];
        wants = [ "sys-kernel-config.mount" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          Environment = [ "PATH=${path}" ];
        };
        script = ''
          modprobe libcomposite || true
          g=/sys/kernel/config/usb_gadget/nix-pixie
          if [ -e "$g/UDC" ] && [ -n "$(cat "$g/UDC")" ]; then
            exit 0
          fi
          mkdir -p "$g"
          echo ${cfg.usbGadget.vendorId} > "$g/idVendor"
          echo ${cfg.usbGadget.productId} > "$g/idProduct"
          echo 0x0100 > "$g/bcdDevice"
          echo 0x0200 > "$g/bcdUSB"
          mkdir -p "$g/strings/0x409"
          echo nix-pixie > "$g/strings/0x409/manufacturer"
          echo "nix-pixie installer" > "$g/strings/0x409/product"
          serial=$(tr -d '\0' < /proc/device-tree/serial-number 2>/dev/null || true)
          echo "''${serial:-0}" > "$g/strings/0x409/serialnumber"
          mkdir -p "$g/configs/c.1/strings/0x409"
          echo "network and serial" > "$g/configs/c.1/strings/0x409/configuration"
          echo 250 > "$g/configs/c.1/MaxPower"
          mkdir -p "$g/functions/ncm.usb0" "$g/functions/acm.GS0"
          [ -e "$g/configs/c.1/ncm.usb0" ] || ln -s "$g/functions/ncm.usb0" "$g/configs/c.1/"
          [ -e "$g/configs/c.1/acm.GS0" ] || ln -s "$g/functions/acm.GS0" "$g/configs/c.1/"
          udc=${if cfg.usbGadget.udc == null then "" else lib.escapeShellArg cfg.usbGadget.udc}
          # The controller's driver may come up a moment later.
          for _ in $(seq 100); do
            if [ -z "$udc" ]; then
              udc=$(ls /sys/class/udc 2>/dev/null | head -n1)
            fi
            if [ -n "$udc" ] && [ -e "/sys/class/udc/$udc" ]; then
              echo "$udc" > "$g/UDC"
              exit 0
            fi
            sleep 0.1
          done
          echo "no USB device controller in /sys/class/udc" >&2
          exit 1
        '';
      };
    };

    boot.initrd.prepend = [ "${toolsArchive}/initrd" ];
    boot.initrd.compressor = lib.mkDefault "zstd";
    # What disko and the install load as they go: device mapper (LUKS,
    # LVM), RAID 1, and the common filesystems. Add the device's own
    # storage and network drivers.
    boot.initrd.availableKernelModules = [
      "dm_mod"
      "dm_crypt"
      "md_mod"
      "raid1"
      "ext4"
      "vfat"
      "nls_cp437"
      "nls_iso8859_1"
    ]
    ++ lib.optionals cfg.usbGadget.enable [
      "libcomposite"
      "usb_f_ncm"
      "usb_f_acm"
    ];

    system.build.nixPixieInstaller = pkgs.runCommand "nix-pixie-installer-initrd" { } ''
      mkdir -p $out
      ln -s ${config.system.build.kernel}/${config.system.boot.loader.kernelFile} $out/kernel
      ln -s ${config.system.build.initialRamdisk}/${config.system.boot.loader.initrdFile} $out/initrd
      echo -n ${lib.escapeShellArg (toString config.boot.kernelParams)} > $out/cmdline
    '';
  };
}
