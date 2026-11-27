# Store copies over another network than the session's (`--bulk-via`): two
# networks between controller and target, the session on the first (found
# at the target's address there), the copy on the second, whose bytes the
# target counts. And when that path isn't there, or breaks, the copy goes
# over the session's.
{
  pkgs,
  package,
  agentModule,
}:
let
  # Doesn't compress (see push.nix), so its size is what goes over the wire.
  blob = pkgs.runCommand "nix-pixie-test-bulk-blob" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    head -c 256M /dev/zero \
      | openssl enc -aes-256-ctr -nosalt -K 1111111111111111111111111111111111111111111111111111111111111111 -iv 00000000000000000000000000000000 > $out
  '';
  mib = 1024 * 1024;
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-bulk";

  nodes = {
    controller = {
      virtualisation.vlans = [
        1
        2
      ];
      environment.systemPackages = [ package ];
      users.users.alice.isNormalUser = true;
      nix.settings.experimental-features = [ "nix-command" ];
      system.extraDependencies = [ blob ];
    };

    target = {
      imports = [ agentModule ];
      virtualisation.vlans = [
        1
        2
      ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
      virtualisation.emptyDiskImages = [
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "pixie-root";
        }
      ];
      environment.systemPackages = [ pkgs.e2fsprogs ];
    };
  };

  testScript = ''
    import json

    def code():
        target.wait_for_file("/run/nix-pixie/code")
        return target.succeed("cat /run/nix-pixie/code").strip()

    def next_code(old):
        target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {old}")

    def received(nic):
        return int(target.succeed(f"cat /sys/class/net/{nic}/statistics/rx_bytes"))

    def push(via, unit=None):
        """Pairs with the target at its address on the first network and
        copies the blob into a fresh /mnt; returns the report and what each
        network carried to the target."""
        current = code()
        target.succeed("rm -rf /mnt/nix")
        before = {nic: received(nic) for nic in ["eth1", "eth2"]}
        cmd = (
            "nix-pixie controller pair --target 192.168.1.2 --yes"
            f" --bulk-via {via} --code {current} --push ${blob} --store-root /mnt"
        )
        if unit is None:
            out = controller.succeed(f"su - alice -c '{cmd} 2>/tmp/push.log'")
        else:
            controller.succeed(
                f"systemd-run --unit={unit} --uid=alice -E HOME=/home/alice -E PATH=/run/current-system/sw/bin"
                f" -p StandardOutput=file:/tmp/{unit}.json -p StandardError=file:/tmp/{unit}.log {cmd}"
            )
            return current, before
        target.succeed("nix-store --store /mnt --check-validity ${blob}")
        carried = {nic: received(nic) - before[nic] for nic in before}
        print(controller.succeed("cat /tmp/push.log"), carried)
        next_code(current)
        return json.loads(out), carried

    start_all()
    target.succeed("mkfs.ext4 -q -L root /dev/disk/by-id/virtio-pixie-root && mkdir -p /mnt && mount LABEL=root /mnt")
    target.wait_for_unit("nix-pixie-agent.service")
    controller.wait_until_succeeds("ping -c1 -W1 192.168.2.2")

    with subtest("to the target's address on the other network"):
        report, carried = push("192.168.2.2")
        assert report["push"]["via"] == "192.168.2.2", report["push"]
        assert carried["eth2"] > 256 * ${toString mib}, carried
        assert carried["eth1"] < 32 * ${toString mib}, carried

    with subtest("auto: an address of the target's other NIC"):
        report, carried = push("auto")
        via = report["push"]["via"]
        assert via in ["192.168.1.2", "192.168.2.2"], report["push"]
        nic = "eth1" if via == "192.168.1.2" else "eth2"
        assert carried[nic] > 256 * ${toString mib}, carried

    with subtest("no such address of the target's: over the session"):
        report, carried = push("192.168.2.9")
        assert report["push"]["via"] is None, report["push"]
        assert "is not an address of this machine" in controller.succeed("cat /tmp/push.log")

    with subtest("the other network goes away midway: the copy finishes over the session"):
        # Slow (on the way to the target), so that the copy is under way
        # when the network goes; then gone both ways. If the session went
        # that way too, it comes back over the first network.
        controller.succeed("tc qdisc add dev eth2 root tbf rate 40mbit burst 64kb latency 500ms")
        current, before = push("192.168.2.2", unit="cut")
        try:
            target.wait_until_succeeds(f"test $(cat /sys/class/net/eth2/statistics/rx_bytes) -gt {before['eth2'] + 32 * ${toString mib}}", timeout=120)
            for machine in [controller, target]:
                machine.succeed("tc qdisc replace dev eth2 root netem loss 100%")
            controller.wait_until_succeeds("systemctl show -P SubState cut | grep -qx -e exited -e failed -e dead", timeout=600)
        finally:
            print(controller.succeed("cat /tmp/cut.log"))
        for machine in [controller, target]:
            machine.succeed("tc qdisc del dev eth2 root")
        output = controller.succeed("cat /tmp/cut.log")
        assert controller.succeed("systemctl show -P Result cut").strip() == "success", output
        assert "the bulk path to the target broke" in output, output
        target.succeed("nix-store --store /mnt --check-validity ${blob}")
        next_code(current)
  '';
}
