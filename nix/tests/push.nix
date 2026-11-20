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
  # One big path: the case where buffering a NAR would hurt most.
  blob = pkgs.runCommand "nix-pixie-test-blob" { } "head -c 512M /dev/zero > $out";
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
