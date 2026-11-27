# The store push: closures go from the controller's store straight onto the
# target's disk, never through RAM.
#
# The agent's service may use at most 192 MiB (systemd kills it beyond that)
# while it receives a 512 MiB store path. If anything along the way buffered a
# whole NAR in memory, the service would die and the push would fail.
{
  pkgs,
  package,
  agentModule,
}:
let
  # One big path: the case where buffering a NAR would hurt most. Pseudorandom
  # (zeros through AES-CTR with a fixed key: the same every build), so it
  # doesn't compress on the wire, and pushing it takes as long as its size says.
  blob = pkgs.runCommand "nix-pixie-test-blob" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    head -c 512M /dev/zero \
      | openssl enc -aes-256-ctr -nosalt -K 0000000000000000000000000000000000000000000000000000000000000000 -iv 00000000000000000000000000000000 > $out
  '';
  # Many small paths with dependencies.
  inherit (pkgs) hello;
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-push";

  nodes = {
    controller = {
      environment.systemPackages = [ package ];
      users.users.alice.isNormalUser = true;
      nix.settings.experimental-features = [ "nix-command" ];
      # Valid in the controller's store, like a freshly built system would be.
      system.extraDependencies = [
        blob
        hello
      ];
    };

    target = {
      imports = [ agentModule ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
      systemd.services.nix-pixie-agent.serviceConfig = {
        MemoryMax = "192M";
        MemorySwapMax = 0;
      };
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "pixie-root";
        }
      ];
      environment.systemPackages = [ pkgs.e2fsprogs ];
    };
  };

  testScript =
    { nodes, ... }:
    ''
      import json
      import time

      def code():
          target.wait_for_file("/run/nix-pixie/code")
          return target.succeed("cat /run/nix-pixie/code").strip()

      def next_code(old):
          target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {old}")

      def pair(current, args, expect_success=True):
          cmd = f"su - alice -c 'nix-pixie controller pair --code {current} --yes {args} 2>&1'"
          if expect_success:
              return controller.succeed(cmd.replace(" 2>&1", ""))
          return controller.fail(cmd)

      start_all()
      # Stand-ins for what disko will do: the target's filesystem at /mnt.
      target.succeed("mkfs.ext4 -q -L root /dev/disk/by-id/virtio-pixie-root && mkdir -p /mnt && mount LABEL=root /mnt")
      target.succeed("mkdir -p /ramdisk /not-mounted && mount -t tmpfs tmpfs /ramdisk")
      target.wait_for_unit("nix-pixie-agent.service")

      with subtest("a store path that isn't there fails before pairing"):
          current = code()
          out = pair(current, "--push /nix/store/00000000000000000000000000000000-nope", expect_success=False)
          assert "not in the local store" in out, out
          assert code() == current, "the code was used"

      with subtest("the target refuses to receive into RAM"):
          current = code()
          out = pair(current, "--push ${hello} --store-root /ramdisk", expect_success=False)
          assert "would be on tmpfs" in out, out
          next_code(current)

      with subtest("the target refuses a root with nothing mounted"):
          current = code()
          out = pair(current, "--push ${hello} --store-root /not-mounted", expect_success=False)
          assert "nothing is mounted at /not-mounted" in out, out
          next_code(current)

      with subtest("a closure bigger than the agent's memory lands on the target's disk"):
          current = code()
          report = json.loads(pair(current, "--push ${blob} --push ${hello}"))
          push = report["push"]
          assert push["root"] == "/mnt" and push["fstype"] == "ext4", push
          print(f"pushed in {push['seconds']:.1f} s")

          # Physically on the disk, and registered in the store under /mnt...
          target.succeed("test $(stat -c %s /mnt${blob}) = 536870912")
          closure = controller.succeed("nix-store -qR ${hello} ${blob}").split()
          target.succeed("nix-store --store /mnt --check-validity " + " ".join(closure))
          # ...and not in the live system's store.
          target.fail("nix-store --check-validity ${blob}")

          restarts = target.succeed("systemctl show -P NRestarts nix-pixie-agent").strip()
          assert restarts == "0", f"the agent was restarted {restarts} times (out of memory?)"
          peak = int(target.succeed("systemctl show -P MemoryPeak nix-pixie-agent").strip())
          print(f"agent memory peak: {peak / 2**20:.0f} MiB (limit 192 MiB, pushed 512+ MiB)")
          next_code(current)

      def push_in_background(unit, current, root):
          # Throttled, so the push takes long enough to get in its way.
          controller.succeed("tc qdisc add dev eth1 root tbf rate 100mbit burst 64kb latency 500ms")
          controller.succeed(
              f"systemd-run --unit={unit} --uid=alice -E HOME=/home/alice -E PATH=/run/current-system/sw/bin"
              + f" -p StandardOutput=file:/tmp/{unit}.json -p StandardError=file:/tmp/{unit}.log"
              + f" nix-pixie controller pair --code {current} --yes --push ${blob} --store-root {root}"
          )
          controller.wait_until_succeeds(f"grep -q pushing /tmp/{unit}.log", timeout=120)
          time.sleep(5)

      def finished(unit):
          controller.wait_until_succeeds(f"systemctl show -P SubState {unit} | grep -qx -e exited -e failed -e dead", timeout=600)
          controller.succeed("tc qdisc del dev eth1 root")
          return controller.succeed(f"systemctl show -P Result {unit}").strip()

      with subtest("an interrupted controller lets the target pair again at once"):
          target.succeed("rm -rf /mnt/nix")
          current = code()
          push_in_background("interrupted", current, "/mnt")
          # Not while a controller is paired: that would end its session.
          refused = target.fail("pixie relay off 2>&1")
          assert "a controller is pairing or paired right now" in refused, refused
          controller.succeed("systemctl kill -s INT interrupted")
          assert finished("interrupted") != "success"
          # Closed on purpose, not lost: the target doesn't wait for it.
          target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {current}", timeout=30)
          target.fail("journalctl -u nix-pixie-agent | grep -q 'waiting for it to come back'")

      with subtest("a push outlives the network going away for longer than a connection does"):
          target.succeed("rm -rf /mnt/nix")
          current = code()
          push_in_background("outage", current, "/mnt")
          # Nothing gets through either way for longer than a silent
          # connection lasts (30 s).
          for machine in [controller, target]:
              machine.succeed("tc qdisc replace dev eth1 root netem loss 100%")
          time.sleep(35)
          target.succeed("tc qdisc del dev eth1 root")
          controller.succeed("tc qdisc replace dev eth1 root tbf rate 100mbit burst 64kb latency 500ms")
          result = finished("outage")
          output = controller.succeed("cat /tmp/outage.log")
          print(output)
          assert result == "success", result
          assert "reconnected; the session goes on" in output, output
          assert "trying again on the new connection" in output, output
          target.succeed("journalctl -u nix-pixie-agent | grep -q 'the controller is back'")
          target.succeed("nix-store --store /mnt --check-validity ${blob}")
          next_code(current)

      with subtest("a whole NixOS system: many small paths"):
          system = "${nodes.controller.system.build.toplevel}"
          paths = controller.succeed(f"nix-store -qR {system}").split()
          size = int(controller.succeed(f"nix path-info -S {system} | cut -f2").strip().split()[-1])
          current = code()
          report = json.loads(pair(current, f"--push {system}"))
          seconds = report["push"]["seconds"]
          print(f"whole system: {len(paths)} paths, {size / 2**20:.0f} MiB in {seconds:.1f} s")
          target.succeed(f"nix-store --store /mnt --check-validity {system}")
          restarts = target.succeed("systemctl show -P NRestarts nix-pixie-agent").strip()
          assert restarts == "0", f"the agent was restarted {restarts} times"
          next_code(current)
    '';
}
