# The shipped installer image, booted with UEFI on one machine: it installs a
# systemd-boot system onto the machine's disk, powers off, and the firmware
# then boots the installed system from that disk, like a real machine whose
# disk comes first in the boot order.
#
# With lanzaboote's source, the system has Secure Boot instead
# (`targets/secure-boot.nix`). The firmware starts in Setup Mode, without
# keys, so the unsigned installer boots, and the installed system enrolls
# its own keys.
#
# The image is `packages.installer-iso` plus the test driver's backdoor (and
# lighter compression). Its network is our installer's: no DHCP server on the
# test network, so the controller finds it, and later the installed system,
# by IPv6 link-local address only.
{
  pkgs,
  package,
  nixpkgs,
  disko,
  lanzaboote ? null,
}:
let
  inherit (pkgs) lib;
  secureBoot = lanzaboote != null;

  flake =
    import ./user-flake.nix
      {
        inherit
          pkgs
          nixpkgs
          disko
          lanzaboote
          ;
      }
      {
        target = if secureBoot then "secure-boot.nix" else "uefi.nix";
        configurations.web1.disks.main = {
          device = ''"/dev/disk/by-pixie/main"'';
          built = "/dev/disk/by-pixie/main";
        };
        machines = ''
          {
            web1.hostKey = {
              mode = "pull";
              publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
            };
          }
        '';
      };

  installer = nixpkgs.lib.nixosSystem {
    modules = [
      ../nixos/installer-iso.nix
      "${pkgs.path}/nixos/modules/testing/test-instrumentation.nix"
      {
        nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
        services.nix-pixie-agent.package = package;
        # Seconds instead of minutes to build; the contents are the same.
        isoImage.squashfsCompression = "zstd -Xcompression-level 1";
      }
    ];
  };
  iso = "${installer.config.system.build.isoImage}/iso/${installer.config.image.fileName}";

  # Python lines for the body of a subtest.
  body = lines: lib.concatMapStrings (line: "    ${line}\n") lines;
  enrollingBoots = lib.optionalString secureBoot (body [
    "# The first boot creates the keys, leaves them on the ESP for"
    "# systemd-boot and reboots (in the same QEMU, which `allow_reboot`"
    "# keeps running); systemd-boot enrolls them and resets."
    "target.connect()"
    "target.connected = False"
    "target.connect()"
  ]);
  bootEntry = body (
    if secureBoot then
      [
        "# lanzaboote writes no boot entry: the firmware boots the disk's"
        "# default path, EFI/BOOT/BOOTX64.EFI (see the README)."
        "assert \"No boot loaders listed in EFI Variables\" in status, status"
      ]
    else
      [
        "# canTouchEfiVariables: the install wrote a boot entry."
        "assert \"Linux Boot Manager\" in status, status"
      ]
  );
  secureBootOn = lib.optionalString secureBoot (body [
    "assert \"Secure Boot: enabled (user)\" in status, status"
    "boots = target.succeed(\"journalctl --list-boots --quiet\").strip().splitlines()"
    "assert len(boots) == 2, boots"
    "# Made on the target, and nowhere else."
    "target.succeed(\"test -s /var/lib/sbctl/keys/db/db.key\")"
  ]);
  toldAboutKeys = lib.optionalString secureBoot (body [
    "# Told while it waited."
    "assert \"prepares its Secure Boot keys and reboots by itself\" in output, output"
  ]);

  install = lib.concatStringsSep " " [
    "nix-pixie controller install --code CODE --machine web1 --disk main=vda"
    "--yes --then poweroff --boot-timeout 300 --boot-check-interval 5"
  ];
in
pkgs.testers.runNixOSTest {
  name = if secureBoot then "nix-pixie-iso-secure-boot" else "nix-pixie-iso-uefi";

  nodes = {
    controller = {
      imports = [ flake.controllerModule ];
      environment.systemPackages = [ package ];
    };

    # The machine: an empty disk (first in the boot order) and the installer
    # image (second), UEFI firmware with its variables kept across starts.
    # Its own configuration only matters for QEMU's command line.
    target = {
      virtualisation = {
        memorySize = 2048;
        useBootLoader = true;
        useEFIBoot = true;
        # Otherwise the variables come from an image of this configuration.
        efi.keepVariables = false;
        useDefaultFilesystems = false;
        diskImage = "./disk.qcow2";
        diskSize = 4096;
        fileSystems."/" = {
          device = "/dev/disk/by-label/unused";
          fsType = "ext4";
        };
        qemu.options = [
          "-drive file=${iso},if=none,id=installer,media=cdrom,readonly=on,format=raw"
          "-device ide-cd,drive=installer,bus=ide.0,bootindex=2"
        ];
      };
    };
  };

  testScript = ''
    import json

    controller.start()
    target.start()
    target.wait_for_unit("nix-pixie-agent.service", timeout=300)
    target.succeed("test -d /sys/firmware/efi")
    ${flake.setupScript}

    def install_output():
        return controller.succeed("cat /tmp/install.out")

    def wait_for_output(text):
        # If the install fails, stop at once.
        controller.wait_until_succeeds(
            f"grep -q '{text}' /tmp/install.out"
            + " || test \"$(systemctl show -P Result pixie-install)\" != success",
            timeout=600,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        assert result == "success", f"the install failed ({result}):\n{install_output()}"

    with subtest("the ISO installs and powers off"):
        target.wait_for_file("/run/nix-pixie/code")
        code = target.succeed("cat /run/nix-pixie/code").strip()
        controller.succeed(
            "systemd-run --unit=pixie-install --remain-after-exit --uid=alice"
            + " --working-directory=/home/alice/repo -E HOME=/home/alice"
            + " -E PATH=/run/current-system/sw/bin"
            + " -p StandardOutput=file:/tmp/install.out -p StandardError=inherit"
            + " " + "${install}".replace("CODE", code)
        )
        wait_for_output("is powered off")
        target.wait_for_shutdown()

    with subtest("the firmware boots the installed system with systemd-boot"):
        target.start(${lib.optionalString secureBoot "allow_reboot=True"})
    ${enrollingBoots}
        target.wait_for_unit("multi-user.target", timeout=300)
        target.fail("test -e /iso")
        source = target.succeed("findmnt -n -o SOURCE /").strip()
        assert source == "/dev/vda2", source
        status = target.succeed("bootctl status 2>&1")
        print(status)
        assert "Product: systemd-boot" in status, status
    ${bootEntry}
    ${secureBootOn}
    with subtest("the controller checks the host key over link-local"):
        controller.wait_until_succeeds(
            "systemctl show -P SubState pixie-install | grep -qx -e exited -e failed",
            timeout=300,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        output = install_output()
        print(output)
        assert result == "success", result
        report = json.loads(next(l for l in reversed(output.splitlines()) if l.lstrip().startswith("{")))
        print(json.dumps(report, indent=2))
        host_key = report["hostKey"]
        recorded = controller.succeed("cat ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        assert host_key["verified"] and host_key["seen"] == " ".join(recorded.split()[:2]), host_key
        assert host_key["address"].startswith("fe80:"), host_key
    ${toldAboutKeys}
  '';
}
