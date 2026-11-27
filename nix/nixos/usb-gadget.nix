# A USB gadget for installing over the USB cable a device was booted
# through: a network function (CDC-NCM) for the install, and a serial port
# (CDC-ACM) for the agent's status. The host sees a new network interface
# and a serial port.
#
# If a gadget is bound to the device controller already (a vendor's serial
# gadget, say), the functions go into it: unbind, add, rebind.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.nix-pixie.usbGadget;

  setup = pkgs.writeShellApplication {
    name = "nix-pixie-usb-gadget";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.kmod
    ];
    text = ''
      modprobe libcomposite 2>/dev/null || true
      gadgets=/sys/kernel/config/usb_gadget
      udc=${lib.escapeShellArg (toString cfg.udc)}
      # The controller's driver may come up a moment later.
      for _ in $(seq 100); do
        if [ -z "$udc" ]; then
          for found in /sys/class/udc/*; do
            [ -e "$found" ] && udc=''${found##*/} && break
          done
        fi
        [ -n "$udc" ] && [ -e "/sys/class/udc/$udc" ] && break
        sleep 0.1
      done
      if [ -z "$udc" ] || [ ! -e "/sys/class/udc/$udc" ]; then
        echo "no USB device controller in /sys/class/udc" >&2
        exit 1
      fi

      # Whose gadget is on it already, if anyone's.
      g=
      for candidate in "$gadgets"/*; do
        if [ -e "$candidate/UDC" ] && [ "$(cat "$candidate/UDC")" = "$udc" ]; then
          g=$candidate
        fi
      done
      if [ -n "$g" ]; then
        echo "adding to the gadget on $udc: $g"
        echo "" > "$g/UDC"
      else
        g=$gadgets/nix-pixie
        mkdir -p "$g/strings/0x409" "$g/configs/c.1/strings/0x409"
        echo ${cfg.vendorId} > "$g/idVendor"
        echo ${cfg.productId} > "$g/idProduct"
        echo 0x0100 > "$g/bcdDevice"
        echo 0x0200 > "$g/bcdUSB"
        echo nix-pixie > "$g/strings/0x409/manufacturer"
        echo "nix-pixie installer" > "$g/strings/0x409/product"
        # Tells devices apart in /dev/serial/by-id on the host.
        serial=$(tr -d '\0' < /proc/device-tree/serial-number 2>/dev/null || true)
        echo "''${serial:-0}" > "$g/strings/0x409/serialnumber"
        echo nix-pixie > "$g/configs/c.1/strings/0x409/configuration"
        echo 250 > "$g/configs/c.1/MaxPower"
      fi
      config=
      for found in "$g"/configs/*; do
        [ -d "$found" ] && config=$found && break
      done

      # A network function and a serial port, unless there are.
      has() {
        for found in "$@"; do
          [ -e "$found" ] && return 0
        done
        return 1
      }
      has "$g"/functions/ncm.* || mkdir "$g/functions/ncm.usb0"
      has "$g"/functions/acm.* "$g"/functions/gser.* || mkdir "$g/functions/acm.GS0"
      for function in "$g"/functions/ncm.* "$g"/functions/acm.* "$g"/functions/gser.*; do
        [ -e "$function" ] || continue
        [ -e "$config/''${function##*/}" ] || ln -s "$function" "$config/"
      done
      echo "$udc" > "$g/UDC"
    '';
  };
in
{
  options.nix-pixie.usbGadget = {
    enable = lib.mkEnableOption ''
      a USB gadget on the device's USB device controller: a network function
      (CDC-NCM) for the install, and a serial port (CDC-ACM, `ttyGS0`) for
      the agent's status and commands. Added to a gadget that is there
      already'';
    address = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "fe80::2";
      description = ''
        A fixed IPv6 link-local address on the gadget's network interface, so
        the controller can be pointed at it: `--target 'fe80::2%<the host's
        interface>'`. Every device is on a link of its own, so they can all
        have the same one.
      '';
    };
    vendorId = lib.mkOption {
      type = lib.types.str;
      default = "0x1d6b";
      description = "The USB vendor id of a gadget made here (default: the Linux Foundation's).";
    };
    productId = lib.mkOption {
      type = lib.types.str;
      default = "0x0104";
      description = "The USB product id of a gadget made here (default: Multifunction Composite Gadget).";
    };
    udc = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "3550000.usb";
      description = "The USB device controller to use (in /sys/class/udc); `null`: the first one.";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = [
      "libcomposite"
      "usb_f_ncm"
      "usb_f_acm"
    ];

    systemd.services.nix-pixie-usb-gadget = {
      description = "nix-pixie: USB gadget (network and serial)";
      wantedBy = [ "multi-user.target" ];
      before = [
        "systemd-networkd.service"
        "nix-pixie-agent.service"
      ];
      after = [ "sys-kernel-config.mount" ];
      wants = [ "sys-kernel-config.mount" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = lib.getExe setup;
      };
    };

    # Gadget links (every u_ether function reports this driver), before the
    # installer's network for wired links. The link drops for a moment
    # whenever the host enumerates the device anew (some do while it
    # flashes firmware): its addresses stay.
    systemd.network.networks."30-nix-pixie-gadget" = {
      matchConfig.Driver = "g_ether";
      address = lib.optional (cfg.address != null) "${cfg.address}/64";
      networkConfig = {
        DHCP = "yes";
        LinkLocalAddressing = "yes";
        IPv6AcceptRA = true;
        ConfigureWithoutCarrier = true;
      };
      linkConfig.RequiredForOnline = "no";
    };

    services.nix-pixie-agent.statusTty.device = lib.mkDefault "ttyGS0";
  };
}
