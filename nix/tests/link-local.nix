# The direct-cable case: a laptop with NetworkManager and a server booted into
# the installer share a link with no DHCP server and no IPv4 addresses. Only
# IPv6 link-local (and the server's IPv4LL, which the laptop doesn't have)
# connects them.
#
# NetworkManager's default profile waits for a DHCP lease, fails the
# activation after 45 s, and retries forever. Each failure deletes the
# link-local address and flushes the neighbour table; the address comes back
# tentative and is usable again ~2 s later. Pairing survives that, a session
# doesn't: its connection never recovers. So the installer sends router
# advertisements on a link nobody else serves, which complete the profile's
# IPv6 setup and keep it up. This test checks that, a whole install over the
# cable, and what NetworkManager does without the advertisements (so we
# notice if that changes), and that the profile we recommend for other live
# systems avoids it too.
{
  pkgs,
  package,
  installerModule,
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
    machines = "{ }";
  };

  # One NIC on VLAN 1, without the test framework's static addresses.
  linkLocalOnly =
    { lib, ... }:
    {
      virtualisation.vlans = lib.mkForce [ ];
      virtualisation.interfaces.eth1 = {
        vlan = 1;
        assignIP = false;
      };
    };
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-link-local";
  # The installer profile adds a nixpkgs overlay.
  node.pkgsReadOnly = false;

  nodes = {
    # The operator's machine: NetworkManager, default firewall.
    laptop = {
      imports = [
        linkLocalOnly
        flake.controllerModule
      ];
      networking.networkmanager.enable = true;
      environment.systemPackages = [ package ];
    };

    # The machine to install, booted into the installer: the same profile as
    # the ISO, which also enables NetworkManager.
    server =
      { lib, modulesPath, ... }:
      {
        imports = [
          linkLocalOnly
          "${modulesPath}/profiles/installation-device.nix"
          installerModule
        ];
        services.nix-pixie-agent.package = package;
        # The test framework sets root's password too.
        users.users.root.initialHashedPassword = lib.mkForce null;
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

    def pair():
        server.wait_for_file("/run/nix-pixie/code")
        code = server.succeed("cat /run/nix-pixie/code").strip()
        report = json.loads(laptop.succeed(f"su - alice -c 'nix-pixie controller pair --yes --code {code}'"))
        assert report["label"] == "server", report
        assert report["addrs"] and all(a.startswith("[fe80::") for a in report["addrs"]), report
        # Wait for the session to end and a fresh code to appear.
        server.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {code}")

    def failed_activations():
        return int(laptop.succeed("journalctl -u NetworkManager | grep -c 'eth1): Activation: failed' || true").strip())

    def link_local_deletions():
        return int(laptop.succeed("grep -c 'Deleted .*eth1 .*inet6 fe80' /tmp/ip-events || true").strip())

    start_all()
    server.wait_for_unit("nix-pixie-agent.service")
    laptop.wait_for_unit("NetworkManager.service")
    controller = laptop
    ${flake.setupScript}

    with subtest("on the installer, networkd and not NetworkManager owns the wired link"):
        server.wait_for_unit("NetworkManager.service")
        server.succeed("nmcli -t -f DEVICE,STATE device status | grep -q '^eth1:unmanaged$'")
        server.succeed("networkctl status eth1 | grep -q 'Network File: .*40-nix-pixie-wired'")

    with subtest("the shared link has no IPv4 on the laptop"):
        laptop.wait_until_succeeds("ip -6 addr show dev eth1 scope link -tentative | grep -q fe80")
        server.wait_until_succeeds("ip -6 addr show dev eth1 scope link | grep -q fe80")
        laptop.fail("ip -4 addr show dev eth1 | grep -q inet")

    # Every address and neighbour change, with timestamps.
    laptop.succeed(
        "systemd-run --unit=ip-monitor -p Environment=PATH=/run/current-system/sw/bin"
        " sh -c 'ip -ts monitor address neigh > /tmp/ip-events'"
    )

    with subtest("pairs over IPv6 link-local"):
        pair()

    with subtest("the installer advertises on the cable, which nobody else serves"):
        # Once the link has been up for 10 s without anybody serving it.
        server.wait_until_succeeds("systemctl is-active nix-pixie-advertise@eth1.service", timeout=60)
        print(server.succeed("journalctl -u nix-pixie-lonely-links -u nix-pixie-advertise@eth1 -o cat"))
        # The test framework's network has DHCP: none there.
        server.fail("systemctl is-active nix-pixie-advertise@eth0.service")

    profile = laptop.succeed("nmcli -g GENERAL.CONNECTION device show eth1").strip()

    with subtest("so NetworkManager's default profile stays up"):
        # A fresh activation, with the advertisements there from the start.
        laptop.succeed(f"nmcli connection up '{profile}'")
        failures, deletions = failed_activations(), link_local_deletions()
        # Longer than NetworkManager's 45 s DHCP timeout.
        laptop.sleep(60)
        assert failed_activations() == failures, "NetworkManager failed the activation"
        assert link_local_deletions() == deletions, laptop.succeed("tail -20 /tmp/ip-events")
        laptop.succeed("nmcli -g GENERAL.STATE device show eth1 | grep -q '^100'")

    with subtest("a whole install over the cable"):
        # Throttled, so that sending the system takes about two minutes: two
        # of NetworkManager's 45 s rounds, which would have dropped the
        # address and the session without the advertisements. Uncompressed,
        # since the throttle is worked out from the closure's size.
        size = int(laptop.succeed(
            "nix --extra-experimental-features nix-command path-info -S"
            " ${flake.prebuilt.web1.system.build.toplevel} | awk '{print $2}'"
        ).strip())
        kbit = size * 8 // 1000 // 120
        laptop.succeed(f"tc qdisc add dev eth1 root tbf rate {kbit}kbit burst 64kb latency 500ms")
        server.wait_for_file("/run/nix-pixie/code")
        code = server.succeed("cat /run/nix-pixie/code").strip()
        failures, deletions = failed_activations(), link_local_deletions()
        status, _ = laptop.execute(
            f"su - alice -c 'cd repo && nix-pixie controller install --code {code} --machine web1"
            " --disk main=vdb --yes --then stay --timeout 90 --no-compression"
            " >/tmp/install.json 2>/tmp/install.err'"
        )
        print(laptop.succeed("cat /tmp/install.err"))
        assert status == 0, f"the install failed ({status})"
        report = json.loads(laptop.succeed("cat /tmp/install.json"))
        laptop.succeed("tc qdisc del dev eth1 root")
        assert report["route"] == "direct", report
        # Two rounds of ~47 s fit into anything longer than 95 s.
        assert report["pushSeconds"] > 95, report
        assert failed_activations() == failures, "NetworkManager failed the activation"
        assert link_local_deletions() == deletions, laptop.succeed("tail -20 /tmp/ip-events")
        server.succeed("test -L /mnt/nix/var/nix/profiles/system")
        server.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {code}")

    with subtest("without the advertisements, NetworkManager drops the link-local address on each failed activation"):
        server.succeed("systemctl stop nix-pixie-lonely-links.service nix-pixie-advertise@eth1.service")
        failures, deletions = failed_activations(), link_local_deletions()
        laptop.succeed(f"nmcli --wait 0 connection up '{profile}'")
        laptop.wait_until_succeeds(
            "test $(journalctl -u NetworkManager | grep -c 'eth1): Activation: failed')"
            f" -gt {failures}",
            timeout=120,
        )
        laptop.wait_until_succeeds("ip -6 addr show dev eth1 scope link -tentative | grep -q fe80")
        assert link_local_deletions() > deletions, laptop.succeed("cat /tmp/ip-events")

    with subtest("pairing works again after the flap"):
        pair()

    with subtest("a link-local-only profile keeps the address stable"):
        profile = laptop.succeed("nmcli -g GENERAL.CONNECTION device show eth1").strip()
        laptop.succeed(
            f"nmcli connection modify '{profile}' ipv4.method disabled ipv6.method link-local"
            f" && nmcli connection up '{profile}'"
        )
        laptop.wait_until_succeeds("ip -6 addr show dev eth1 scope link -tentative | grep -q fe80")
        failures, deletions = failed_activations(), link_local_deletions()
        # Longer than NetworkManager's 45 s DHCP timeout.
        laptop.sleep(60)
        assert failed_activations() == failures, "NetworkManager still fails the activation"
        assert link_local_deletions() == deletions, laptop.succeed("tail -20 /tmp/ip-events")
        laptop.succeed("nmcli -g GENERAL.STATE device show eth1 | grep -q '^100'")
        pair()
  '';
}
