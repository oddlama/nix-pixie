# The direct-cable case: a laptop with NetworkManager and a server booted into
# the installer share a link with no DHCP server and no IPv4 addresses. Only
# IPv6 link-local (and the server's IPv4LL, which the laptop doesn't have)
# connects them.
#
# NetworkManager's default profile waits for a DHCP lease, fails the
# activation after 45 s, and retries forever. Each failure deletes the
# link-local address and flushes the neighbour table; the address comes back
# tentative and is usable again ~2 s later. This test pins that behaviour
# down (so we notice if it changes), checks that pairing survives it, and
# checks that the profile we recommend for direct cables avoids it.
{
  pkgs,
  package,
  installerModule,
}:
let
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
      imports = [ linkLocalOnly ];
      networking.networkmanager.enable = true;
      environment.systemPackages = [ package ];
      users.users.alice.isNormalUser = true;
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

    with subtest("NetworkManager's default profile drops the link-local address on each failed activation"):
        laptop.wait_until_succeeds("test $(journalctl -u NetworkManager | grep -c 'eth1): Activation: failed') -ge 1", timeout=120)
        laptop.wait_until_succeeds("ip -6 addr show dev eth1 scope link -tentative | grep -q fe80")
        assert link_local_deletions() >= 1, laptop.succeed("cat /tmp/ip-events")

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
