# The whole install: pairing to a booted system whose SSH host key the
# controller checks. The target generates the key (pull mode) and powers off
# when done; then a second VM boots the disk the target installed onto,
# like the target would after the operator removes the installer medium.
#
# Node names matter: nodes are numbered alphabetically, so `target` is
# 192.168.1.3, which the installed system takes over (`install-target.nix`).
{
  pkgs,
  package,
  agentModule,
  disko,
}:
let
  flake = import ./user-flake.nix { inherit pkgs disko; } {
    targetModule = ./install-target.nix;
    configurations.web1 = {
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
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-install";

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
      # Becomes empty0.qcow2 in the target's state directory.
      virtualisation.emptyDiskImages = [
        {
          size = 3072;
          driveConfig.deviceExtraOpts.serial = "pixie-a";
        }
      ];
    };

    # Boots the target's install disk with its own boot loader. Its own
    # configuration only matters for QEMU's command line.
    installed = {
      virtualisation.useBootLoader = true;
      virtualisation.useDefaultFilesystems = false;
      virtualisation.diskImage = "./empty0.qcow2";
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
            + " --disk main=vdb --yes --then poweroff --boot-timeout 600"
        )
        target.wait_for_shutdown()
        recorded = controller.succeed("cat ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        recorded_key = " ".join(recorded.split()[:2])
        assert recorded.strip().endswith(" root@web1"), recorded
        staged = controller.succeed("su - alice -c 'cd repo && git diff --cached --name-only'")
        assert "machines/web1/ssh_host_ed25519_key.pub" in staged, staged

    with subtest("the installed system boots from the disk"):
        installed.state_dir = target.state_dir
        installed.start()
        installed.wait_for_unit("multi-user.target")
        assert installed.succeed("findmnt -n -o SOURCE /").strip() == "/dev/vda2"
        # The udev rule recreates the agnostic disk's path, for grub-install.
        assert installed.succeed("readlink -f /dev/disk/by-pixie/main").strip() == "/dev/vda"

    with subtest("the controller checks the host key it pulled"):
        controller.wait_until_succeeds(
            "systemctl show -P SubState pixie-install | grep -qx -e exited -e failed",
            timeout=600,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        report = json.loads(controller.succeed("cat /tmp/install.json"))
        print(json.dumps(report, indent=2))
        assert result == "success", result
        host_key = report["hostKey"]
        assert host_key["verified"] and host_key["seen"] == recorded_key, host_key
        assert host_key["address"] == "192.168.1.3", host_key

    with subtest("the key is where the configuration wants it, private"):
        public = installed.succeed("cat /etc/ssh/ssh_host_ed25519_key.pub")
        assert " ".join(public.split()[:2]) == recorded_key, public
        assert installed.succeed("stat -c %a /etc/ssh/ssh_host_ed25519_key").strip() == "600"

    with subtest("nixos-install copied nothing beyond the pushed system"):
        valid = set(installed.succeed("nix --extra-experimental-features nix-command path-info --all").split())
        closure = set(installed.succeed("nix-store -qR /run/current-system").split())
        assert valid == closure, f"extra: {sorted(valid - closure)}, missing: {sorted(closure - valid)}"
  '';
}
