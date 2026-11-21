# The whole install of one layout (see `layouts.nix`): pairing to a booted
# system whose SSH host key the controller checks. The target generates the
# key (pull mode) and powers off when done; then a second VM boots the disks
# the target installed onto, like the target would after the operator
# removes the installer medium.
#
# Node names matter: nodes are numbered alphabetically, so `target` is
# 192.168.1.3, which the installed system takes over (`targets/base.nix`).
{
  pkgs,
  package,
  agentModule,
  nixpkgs,
  disko,
  layout,
}:
let
  inherit (pkgs) lib;
  spec = {
    zfs = false;
    hostKeyPath = "/etc/ssh/ssh_host_ed25519_key";
  }
  // (import ./layouts.nix).${layout};
  byPixie = disk: "/dev/disk/by-pixie/${disk}";

  flake = import ./user-flake.nix { inherit pkgs nixpkgs disko; } {
    target = "${layout}.nix";
    configurations.web1.disks = lib.genAttrs spec.disks (disk: {
      device = ''"${byPixie disk}"'';
      built = byPixie disk;
    });
    machines = ''
      {
        web1.hostKey = {
          mode = "pull";
          publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
        };
      }
    '';
  };

  # The disko disks in order, with the disk each gets on the target (after
  # its own root, vda) and on the installed machine.
  disks = lib.imap0 (i: disk: {
    name = disk;
    onTarget = "vd${lib.elemAt [ "b" "c" ] i}";
    installed = "/dev/vd${lib.elemAt [ "a" "b" ] i}";
  }) spec.disks;
  diskArgs = lib.concatMapStringsSep " " (d: "--disk ${d.name}=${d.onTarget}") disks;
in
assert lib.length spec.disks <= 2;
pkgs.testers.runNixOSTest {
  name = "nix-pixie-install-${layout}";

  nodes = {
    controller = {
      imports = [ flake.controllerModule ];
      environment.systemPackages = [ package ];
    };

    target = {
      imports = [
        agentModule
        ../nixos/storage.nix
      ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
      # Become empty0.qcow2 and empty1.qcow2 in the target's state directory.
      virtualisation.emptyDiskImages = [
        {
          size = 3072;
          driveConfig.deviceExtraOpts.serial = "pixie-a";
        }
        {
          size = 3072;
          driveConfig.deviceExtraOpts.serial = "pixie-b";
        }
      ];
      boot.supportedFilesystems.zfs = spec.zfs;
      # The installer's own; the installed system has another.
      networking.hostId = lib.mkIf spec.zfs "c0ffee00";
    };

    # Boots the target's disks with their own boot loader. Its own
    # configuration only matters for QEMU's command line.
    installed = {
      virtualisation.useBootLoader = true;
      virtualisation.useDefaultFilesystems = false;
      virtualisation.diskImage = "./empty0.qcow2";
      virtualisation.qemu.options = [ "-drive file=./empty1.qcow2,if=virtio,format=qcow2" ];
      virtualisation.fileSystems."/" = {
        device = "/dev/disk/by-label/unused";
        fsType = "ext4";
      };
    };
  };

  testScript = ''
    import json

    controller.start()
    target.start()
    target.wait_for_unit("nix-pixie-agent.service")
    target.succeed("ip -4 addr show eth1 | grep -q 'inet 192.168.1.3/24'")
    ${flake.setupScript}

    with subtest("install, recording the generated host key, then power off"):
        target.wait_for_file("/run/nix-pixie/code")
        code = target.succeed("cat /run/nix-pixie/code").strip()
        # In the background: after the install, it waits for the system to boot.
        controller.succeed(
            "systemd-run --unit=pixie-install --remain-after-exit --uid=alice"
            + " --working-directory=/home/alice/repo -E HOME=/home/alice"
            + " -E PATH=/run/current-system/sw/bin"
            + " -p StandardOutput=file:/tmp/install.json -p StandardError=journal"
            + f" nix-pixie controller install --code {code} --machine web1"
            + " ${diskArgs} --yes --then poweroff --boot-timeout 300"
        )
        # The target powers off once installed. If the install fails, it never
        # does: stop at once instead of waiting for the test's timeout.
        controller.wait_until_succeeds(
            "journalctl -o cat -u pixie-install | grep -q 'is powered off'"
            + " || test \"$(systemctl show -P Result pixie-install)\" != success",
            timeout=600,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        assert result == "success", f"the install failed ({result}); its output is above"
        target.wait_for_shutdown()
        recorded = controller.succeed("cat ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        recorded_key = " ".join(recorded.split()[:2])
        assert recorded.strip().endswith(" root@web1"), recorded
        staged = controller.succeed("su - alice -c 'cd repo && git diff --cached --name-only'")
        assert "machines/web1/ssh_host_ed25519_key.pub" in staged, staged

    with subtest("the installed system boots from its disks"):
        installed.state_dir = target.state_dir
        installed.start()
        installed.wait_for_unit("multi-user.target", timeout=300)
        # The udev rule recreates the agnostic disks' paths, for grub-install.
    ${lib.concatMapStrings (
      d: "    assert installed.succeed(\"readlink -f ${byPixie d.name}\").strip() == \"${d.installed}\"\n"
    ) disks}

    with subtest("the controller checks the host key it pulled"):
        controller.wait_until_succeeds(
            "systemctl show -P SubState pixie-install | grep -qx -e exited -e failed",
            timeout=300,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        report = json.loads(controller.succeed("cat /tmp/install.json"))
        print(json.dumps(report, indent=2))
        assert result == "success", result
        host_key = report["hostKey"]
        assert host_key["verified"] and host_key["seen"] == recorded_key, host_key
        assert host_key["address"] == "192.168.1.3", host_key

    with subtest("the key is where the configuration wants it, private"):
        public = installed.succeed("cat ${spec.hostKeyPath}.pub")
        assert " ".join(public.split()[:2]) == recorded_key, public
        assert installed.succeed("stat -c %a ${spec.hostKeyPath}").strip() == "600"

    with subtest("nixos-install copied nothing beyond the pushed system"):
        valid = set(installed.succeed("nix --extra-experimental-features nix-command path-info --all").split())
        closure = set(installed.succeed("nix-store -qR /run/current-system").split())
        assert valid == closure, f"extra: {sorted(valid - closure)}, missing: {sorted(closure - valid)}"

    with subtest("the layout is what the configuration describes"):
    ${lib.concatMapStrings (line: "    ${line}\n") (lib.splitString "\n" spec.checks)}
  '';
}
