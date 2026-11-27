# The netboot installer on a board that boots over USB, as far as QEMU goes.
#
# It starts from kernel and initrd alone, with a command line that has
# nothing of ours: just a console, and a `root=` naming a disk, as boards'
# own command lines do. Its USB gadget is on dummy_hcd, a USB device
# controller whose host side is the same machine: the gadget's network
# interface and serial port show up there as the other end of the cable
# would see them, so a controller on the same machine pairs with the agent
# over them.
{
  pkgs,
  package,
  nixpkgs,
}:
let
  inherit (pkgs) lib;
  qemu-common = import "${pkgs.path}/nixos/lib/qemu-common.nix" {
    inherit (pkgs) lib stdenv;
  };

  installer = nixpkgs.lib.nixosSystem {
    modules = [
      ../nixos/installer-netboot.nix
      "${pkgs.path}/nixos/modules/testing/test-instrumentation.nix"
      {
        nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
        services.nix-pixie-agent = {
          inherit package;
          # No relay in reach here, and the code comes by cable anyway.
          relay.enable = false;
        };
        systemd.services.nix-pixie-agent.environment.RUST_LOG = "nix_pixie=debug";
        nix-pixie.usbGadget.enable = true;
        boot.kernelModules = [ "dummy_hcd" ];
        nix-pixie.netboot.kernelModules = [
          "virtio_pci"
          "virtio_net"
          "virtio_blk"
          "virtio_console"
          "virtio_rng"
          "dummy_hcd"
          # The host's side of the cable.
          "cdc_ncm"
          "cdc_acm"
          "kernel/drivers/usb/gadget"
        ];
        # Seconds instead of minutes to build; the contents are the same.
        netboot.squashfsCompression = "zstd -Xcompression-level 1";
      }
    ];
  };
  netboot = installer.config.system.build.nixPixieNetboot;

  # The driver adds its console, the serial port and -no-reboot.
  startCommand = lib.concatStringsSep " " [
    "${pkgs.coreutils}/bin/truncate -s 1G disk.img &&"
    "exec ${qemu-common.qemuBinary pkgs.qemu_test} -m 3072 -smp 2"
    "-kernel ${netboot}/kernel -initrd ${netboot}/initrd"
    "-append 'console=ttyS0 root=/dev/vda'"
    "-drive file=disk.img,if=virtio,format=raw"
    "-netdev user,id=net0 -device virtio-net-pci,netdev=net0"
  ];

  exe = "${package}/bin/nix-pixie";
  udc = "/sys/kernel/config/usb_gadget/nix-pixie/UDC";
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-netboot";
  nodes = { };

  testScript = ''
    import json
    import shlex
    import time

    board = create_machine("${startCommand}", name="board")
    board.start()
    board.wait_for_unit("multi-user.target", timeout=300)

    # Machines made here are the script's to stop, even when it fails.
    try:
        with subtest("it boots from kernel and initrd alone, whatever root= says"):
            cmdline = board.succeed("cat /proc/cmdline").strip()
            assert cmdline == "console=ttyS0 root=/dev/vda", cmdline
            root = board.succeed("findmnt -n -o FSTYPE /").strip()
            assert root == "tmpfs", root
            board.succeed("test -b /dev/vda")
            board.fail("findmnt -S /dev/vda")
            failed = board.succeed("systemctl list-units --failed --plain --no-legend").strip()
            assert failed == "", failed
            # The installer's own modules, and only the board's besides.
            board.succeed("modinfo dm_crypt xts af_packet nf_tables usb_f_ncm")
            board.fail("modinfo snd_hda_intel")

        with subtest("the USB gadget: a network interface and a serial port"):
            board.wait_for_unit("nix-pixie-usb-gadget.service")
            board.wait_until_succeeds("test -c /dev/ttyGS0 && test -c /dev/ttyACM0")
            board.wait_until_succeeds("ip -6 addr show dev usb0 | grep -q 'fe80::2/64'")
            network = board.succeed("networkctl status usb0 --no-pager")
            assert "30-nix-pixie-gadget.network" in network, network
            # A cable to one host: advertised on at once.
            board.wait_for_unit("nix-pixie-advertise@usb0.service")
            board.wait_for_unit("nix-pixie-status.service")
            host = board.succeed(
                "for d in /sys/class/net/*; do"
                " test \"$(basename \"$(readlink -f $d/device/driver)\")\" = cdc_ncm && basename $d;"
                " done; true"
            ).strip()
            assert host, "no host side for the gadget's network"

        def to_port(text):
            board.succeed(f"printf %s {shlex.quote(text)} > /dev/ttyACM0")

        with subtest("the serial status terminal writes it all again, and answers no echoes"):
            # The host's end, held open raw, as a program would.
            board.succeed("stty -F /dev/ttyACM0 raw -echo clocal")
            board.succeed(
                "systemd-run --unit=acm-reader -E PATH=/run/current-system/sw/bin"
                " sh -c 'exec cat /dev/ttyACM0 > /tmp/acm.out'"
            )
            # Asked for until it comes: what arrives while the console doesn't
            # read (between two of its runs, say) is lost.
            for _ in range(60):
                to_port("\r\n")
                if board.execute("grep -aq '^code [0-9]*-' /tmp/acm.out")[0] == 0:
                    break
                time.sleep(1)
            else:
                raise AssertionError("the status terminal wrote no code")
            status = board.succeed("grep -a -m1 '^code [0-9]*-' /tmp/acm.out").strip()
            # Its own lines back, and escape sequences: as from a host that
            # echoes. Each command after them is answered once they are read.
            to_port(f"{status}\r\n\x1b[1mfoo\r\nhelp\r\n")
            board.wait_until_succeeds("grep -aq '^> help: ' /tmp/acm.out", timeout=60)
            answer = board.succeed("grep -a -m1 '^> help: ' /tmp/acm.out").strip()
            # (The console echoes what it reads, so the answer is there twice
            # then: another command's answer shows it was read.)
            to_port(f"{answer}\r\nrelay\r\n")
            board.wait_until_succeeds("grep -aq '^> relay: which relays' /tmp/acm.out", timeout=60)
            out = board.succeed("cat /tmp/acm.out")
            assert "no such command" not in out, repr(out)
            assert "which code" not in out, repr(out)
            board.succeed("systemctl stop acm-reader")

        with subtest("a controller started before the device hands it the code over the serial port"):
            # Unplugged: no interface or serial port on the host's side.
            board.succeed("echo > ${udc}")
            board.wait_until_fails(f"test -e /sys/class/net/{host}")
            board.wait_until_fails("test -e /dev/ttyACM0")
            board.succeed(
                "systemd-run --unit=pixie-pair -E HOME=/root -E NO_COLOR=1 -E PATH=/run/current-system/sw/bin"
                " -E RUST_LOG=nix_pixie=debug"
                " -p StandardOutput=file:/tmp/pair.json -p StandardError=file:/tmp/pair.log"
                f" ${exe} controller pair --serial /dev/ttyACM0 --target fe80::2%{host}"
                " --yes --timeout 120"
            )
            board.wait_until_succeeds(f"grep -q 'no interface {host} yet' /tmp/pair.log")
            board.wait_until_succeeds("grep -q 'waiting for the serial port' /tmp/pair.log")
            # Plugged in: a new interface (index) of the same name, and the port.
            board.succeed("echo dummy_udc.0 > ${udc}")
            board.wait_until_succeeds(
                "test \"$(systemctl show -P SubState pixie-pair)\" != running", timeout=180
            )
            pairing = board.succeed("cat /tmp/pair.log")
            print(pairing)
            result = board.succeed("systemctl show -P Result pixie-pair").strip()
            if result != "success":
                print(board.succeed("journalctl -u nix-pixie-agent -o cat | tail -60"))
            assert result == "success", result
            assert "answer=waiting for the controller with that code" in pairing, pairing
            report = json.loads(board.succeed("cat /tmp/pair.json"))
            assert report["target"] == board.succeed("cat /run/nix-pixie/endpoint-id").strip(), report
            board.succeed("journalctl -u nix-pixie-agent | grep -q 'paired with controller'")

        with subtest("codes given in a row, the agent listening anew each time"):
            # The agent waits for that controller again: the code lasts the boot.
            board.wait_until_succeeds("journalctl -u nix-pixie-agent | grep -q 'code was given earlier'")
            restarts = board.succeed("systemctl show -P NRestarts nix-pixie-agent").strip()
            answers = [
                board.succeed("pixie code default").strip(),
                board.succeed("pixie code acid-acorn-acre").strip(),
                board.succeed("pixie code acid-acorn-acre").strip(),
                board.succeed("pixie code default").strip(),
            ]
            assert answers == [
                "the target shows a code of its own again",
                "waiting for the controller with that code",
                "already waiting for the controller with that code",
                "the target shows a code of its own again",
            ], answers
            refused = board.fail("pixie code 482913-acid-acorn-acre 2>&1")
            assert "target's" in refused, refused
            after = board.succeed("systemctl show -P NRestarts nix-pixie-agent").strip()
            assert after == restarts, f"the agent was restarted ({restarts} -> {after})"
            board.wait_for_file("/run/nix-pixie/code")
    except Exception:
        print(board.execute("journalctl -b -u nix-pixie-agent -u nix-pixie-status -o cat | tail -80")[1])
        print(board.execute("grep -v 'status terminal' /tmp/pair.log | tail -40")[1])
        print(repr(board.execute("cat /tmp/acm.out")[1]))
        raise
    finally:
        board.crash()
  '';
}
