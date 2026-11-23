# A target on another network, booted into a live system that isn't our
# installer: a server at a hosting provider running its stock NixOS ISO.
#
# The controller and the target sit on different subnets behind a router,
# so neither broadcasts nor mDNS reach. The target is a plain NixOS with its
# firewall, running the agent package as `nix run github:oddlama/nix-pixie#agent`
# would (prebuilt here: the VM can't fetch). The controller finds it only
# with `--target` and installs across the router.
#
# Nodes are numbered alphabetically: controller 1, router 2, target 3.
{
  pkgs,
  package,
  agent,
  nixpkgs,
  disko,
}:
let
  flake = import ./user-flake.nix { inherit pkgs nixpkgs disko; } {
    target = "minimal.nix";
    configurations.web1.disks.main = {
      device = ''"/dev/disk/by-pixie/main"'';
      built = "/dev/disk/by-pixie/main";
    };
    machines = ''
      {
        web1 = {
          facter.reportPath = "machines/web1/facter.json";
          hostKey = {
            mode = "pull";
            publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
          };
        };
      }
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-remote";

  nodes = {
    controller = {
      imports = [ flake.controllerModule ];
      environment.systemPackages = [ package ];
      networking.interfaces.eth1.ipv4.routes = [
        {
          address = "192.168.2.0";
          prefixLength = 24;
          via = "192.168.1.2";
        }
      ];
    };

    router = {
      virtualisation.vlans = [
        1
        2
      ];
      boot.kernel.sysctl."net.ipv4.ip_forward" = 1;
    };

    # A plain NixOS with its default firewall, none of our modules.
    target = {
      virtualisation.vlans = [ 2 ];
      networking.interfaces.eth1.ipv4.routes = [
        {
          address = "192.168.1.0";
          prefixLength = 24;
          via = "192.168.2.2";
        }
      ];
      environment.systemPackages = [ agent ];
      users.users.alice.isNormalUser = true;
      virtualisation.emptyDiskImages = [
        {
          size = 3072;
          driveConfig.deviceExtraOpts.serial = "pixie-a";
        }
      ];
    };
  };

  testScript = ''
    import json

    start_all()
    target.wait_for_unit("multi-user.target")
    controller.wait_for_unit("multi-user.target")
    controller.wait_until_succeeds("ping -c1 -W2 192.168.2.3")
    ${flake.setupScript}

    with subtest("the agent package runs on a plain NixOS and opens its firewall"):
        out = target.fail("su - alice -c nix-pixie-agent 2>&1")
        assert "run it as root" in out, out
        target.succeed("systemd-run --unit=pixie-agent nix-pixie-agent")
        target.wait_for_file("/run/nix-pixie/code")
        rules = target.succeed("iptables -S nixos-fw")
        assert "--dport 47011" in rules and "--dport 47012" in rules, rules
        # mDNS would make a server on the internet a reflector.
        assert "--dport 5353" not in rules, rules
        code = target.succeed("cat /run/nix-pixie/code").strip()

    with subtest("the local network doesn't have it"):
        out = controller.fail(f"su - alice -c 'cd repo && nix-pixie controller pair --code {code} --timeout 5' 2>&1")
        assert "no target with nameplate" in out, out

    with subtest("--target installs it across the router"):
        report = json.loads(controller.succeed(
            f"su - alice -c 'cd repo && nix-pixie controller install --code {code}"
            + " --target 192.168.2.3 --machine web1 --disk main=vdb --yes --then stay"
            + " 2>/tmp/install.err'"
        ))
        print(controller.succeed("cat /tmp/install.err"))
        assert report["disks"][0]["disk"] == "vdb", report
        # The machine's facter report didn't exist yet: no evaluation before pairing.
        controller.succeed("grep -q 'Not evaluating nixosConfigurations.web1 before pairing' /tmp/install.err")
        controller.succeed("test -s ~alice/repo/machines/web1/facter.json")
        controller.succeed("test -s ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        target.succeed("test -L /mnt/nix/var/nix/profiles/system")
        target.succeed("test -s /mnt/boot/grub/grub.cfg")
  '';
}
