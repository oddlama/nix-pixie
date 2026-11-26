# `nix-pixie controller install` up to a formatted, mounted disk with the
# system on it: disk choice in both modes, the agnostic link and its udev
# rule, recording pinned disks in the repository, and the protections around
# erasing data and against another disk carrying the same partition labels.
#
# The controller evaluates the user's flake inside its VM (see
# `user-flake.nix`) and stays in the installer afterwards (`--then stay`).
{
  pkgs,
  package,
  agentModule,
  nixpkgs,
  disko,
}:
let
  flake = import ./user-flake.nix { inherit pkgs nixpkgs disko; } {
    target = "minimal.nix";
    configurations = {
      # Pinned: the controller records the disk in the repository. QEMU
      # names virtio disks by their serial.
      web1.disks.main = {
        device = ''pixieLib.pinnedDisk ./machines/web1/disks.json "main"'';
        built = "/dev/disk/by-id/virtio-pixie-b";
      };
      # Agnostic: any machine, the disk is chosen at install time.
      fleet.disks.main = {
        device = ''"/dev/disk/by-pixie/main"'';
        built = "/dev/disk/by-pixie/main";
      };
    };
    machines = ''
      {
        web1.disks.file = "machines/web1/disks.json";
        fleet = { };
      }
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-disko";

  nodes = {
    controller = {
      imports = [ flake.controllerModule ];
      environment.systemPackages = [ package ];
    };

    target = {
      imports = [ agentModule ];
      virtualisation.memorySize = 1536;
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
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
      # What an installed agnostic system has: the rule that recreates
      # /dev/disk/by-pixie/main from the partition labels.
      services.udev.packages = [ flake.prebuilt.fleet.system.build.nixPixieDiskRules ];
    };
  };

  testScript = ''
    import json

    def code():
        target.wait_for_file("/run/nix-pixie/code")
        return target.succeed("cat /run/nix-pixie/code").strip()

    def next_code(old):
        target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {old}")

    def install(args, expect_success=True):
        current = code()
        # Stay in the installer: the test inspects what is mounted.
        cmd = f"su - alice -c 'cd repo && nix-pixie controller install --code {current} --then stay {args}'"
        if expect_success:
            report = json.loads(controller.succeed(cmd))
            next_code(current)
            return report
        out = controller.fail(cmd + " 2>&1")
        return current, out

    def mounted_from(path):
        return target.succeed(f"findmnt -n -o SOURCE --target {path}").strip()

    start_all()
    target.wait_for_unit("nix-pixie-agent.service")
    ${flake.setupScript}

    with subtest("a role that doesn't exist fails before pairing"):
        current, out = install("--machine fleet --disk nope=vdb --yes", expect_success=False)
        assert "no such role" in out, out
        assert code() == current, "the code was used"

    with subtest("with two free disks, --yes doesn't guess"):
        current, out = install("--machine fleet --yes", expect_success=False)
        assert "choose the disk for main" in out, out
        # The candidates: both empty disks, never the VM's own root disk.
        candidates = out.split("Candidates:")[1]
        assert "vdb" in candidates and "vdc" in candidates, candidates
        assert "vda" not in candidates, candidates
        next_code(current)

    with subtest("agnostic: formats the chosen disk and sends the system"):
        report = install("--machine fleet --disk main=pixie-a --yes")
        [disk] = report["disks"]
        assert disk["disk"] == "vdb" and disk["link"] and disk["pin"] is None, disk
        assert mounted_from("/mnt") == "/dev/vdb2", mounted_from("/mnt")
        assert target.succeed("readlink /dev/disk/by-pixie/main").strip() == "../../vdb"
        target.succeed(f"nix-store --store /mnt --check-validity {report['system']}")
        target.succeed("test -b /dev/disk/by-partlabel/disk-main-root")

    with subtest("the installed system's udev rule recreates the link"):
        target.succeed("rm /dev/disk/by-pixie/main")
        target.succeed("udevadm trigger --subsystem-match=block --action=change && udevadm settle")
        assert target.succeed("readlink -f /dev/disk/by-pixie/main").strip() == "/dev/vdb"

    with subtest("an old disk with the same layout blocks the install"):
        # vdb still holds the agnostic install, whose partitions carry the
        # labels web1's layout creates: disko could mount those instead.
        current, out = install("--machine web1 --disk main=vdc --yes", expect_success=False)
        assert "vdb (disk-main-boot, disk-main-root)" in out, out
        controller.fail("test -e ~alice/repo/machines/web1/disks.json")
        next_code(current)
        # What the operator would do: erase the old install.
        target.succeed("umount -R /mnt && wipefs --all /dev/vdb2 /dev/vdb1 /dev/vdb && udevadm settle")

    with subtest("pinned: records the chosen disk in the repository"):
        report = install("--machine web1 --disk main=vdc --yes")
        [disk] = report["disks"]
        assert disk["disk"] == "vdc" and not disk["link"], disk
        assert disk["pin"] == "/dev/disk/by-id/virtio-pixie-b", disk
        recorded = json.loads(controller.succeed("cat ~alice/repo/machines/web1/disks.json"))
        assert recorded == {"main": "/dev/disk/by-id/virtio-pixie-b"}, recorded
        staged = controller.succeed("su - alice -c 'cd repo && git diff --cached --name-only'")
        assert "machines/web1/disks.json" in staged, staged
        assert mounted_from("/mnt") == "/dev/vdc2", mounted_from("/mnt")
        target.succeed(f"nix-store --store /mnt --check-validity {report['system']}")
        target.succeed("touch /mnt/precious")

    with subtest("data on a disk needs --wipe"):
        # The recorded disk is chosen again, and it now holds data.
        current, out = install("--machine web1 --yes", expect_success=False)
        assert "pass --wipe" in out, out
        assert "HAS DATA" in out, out
        target.succeed("test -e /mnt/precious")
        next_code(current)

    with subtest("--wipe reinstalls onto the recorded disk, mounted, after the agent restarted"):
        # The agent still knows the disk is its own doing, though its
        # filesystems are mounted (disko unmounts them).
        old = target.succeed("cat /run/nix-pixie/code").strip()
        target.succeed("systemctl restart nix-pixie-agent")
        next_code(old)
        target.succeed("grep -qx vdc /run/nix-pixie/formatted")
        assert mounted_from("/mnt") == "/dev/vdc2", mounted_from("/mnt")
        report = install("--machine web1 --yes --wipe")
        assert report["disks"][0]["disk"] == "vdc", report
        assert mounted_from("/mnt") == "/dev/vdc2", mounted_from("/mnt")
        target.fail("test -e /mnt/precious")
  '';
}
