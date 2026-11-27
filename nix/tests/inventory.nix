# After pairing: the target's inventory, matching it against the user's flake,
# approval (and rejection), and the facter report landing in the repository.
{
  pkgs,
  package,
  agentModule,
  pixieLib,
}:
let
  # The user's flake. Its `pixie` output comes from our own lib, so the
  # controller parses exactly what real flakes produce.
  pixieJson = pkgs.writeText "pixie.json" (
    builtins.toJSON (
      pixieLib.mkPixie {
        # Only the attribute names matter for the metadata.
        nixosConfigurations = {
          web1 = null;
          spare = null;
        };
        config.machines = {
          web1 = {
            # The target's NIC on VLAN 1 (QEMU's MAC scheme: VLAN 1, node 2).
            match.mac = [ "52:54:00:12:01:02" ];
            facter.reportPath = "machines/web1/facter.json";
          };
          spare.match.dmiSerial = [ "no-such-serial" ];
        };
      }
    )
  );
  flakeNix = pkgs.writeText "flake.nix" ''
    {
      outputs = _: {
        pixie = builtins.fromJSON (builtins.readFile ./pixie.json);
      };
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-inventory";

  nodes = {
    controller = {
      environment.systemPackages = [
        package
        pkgs.git
      ];
      users.users.alice.isNormalUser = true;
      nix.settings = {
        experimental-features = [
          "nix-command"
          "flakes"
        ];
        # Offline: don't try to fetch the global flake registry.
        flake-registry = "";
      };
    };

    target = {
      imports = [ agentModule ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
      # A second disk with a serial, so it shows up in /dev/disk/by-id.
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "pixie-data";
        }
      ];
      environment.systemPackages = [ pkgs.e2fsprogs ];
    };
  };

  testScript = ''
    import json

    def as_alice(cmd):
        return f"su - alice -c '{cmd}'"

    def code():
        target.wait_for_file("/run/nix-pixie/code")
        return target.succeed("cat /run/nix-pixie/code").strip()

    def next_code(old):
        target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {old}")

    start_all()

    # Leftovers from an earlier install on the second disk.
    target.succeed("echo 'label: gpt' | sfdisk /dev/disk/by-id/virtio-pixie-data")
    target.succeed("echo ',,L' | sfdisk --append /dev/disk/by-id/virtio-pixie-data")
    target.succeed("udevadm settle && mkfs.ext4 -q -L old-root /dev/disk/by-id/virtio-pixie-data-part1 && udevadm settle")

    controller.succeed(as_alice(
        "mkdir infra && cd infra && cp ${flakeNix} flake.nix && cp ${pixieJson} pixie.json"
        " && chmod u+w flake.nix pixie.json && git init -q && git add ."
    ))
    target.wait_for_unit("nix-pixie-agent.service")

    with subtest("a broken --machine fails before the code is used"):
        current = code()
        out = controller.fail(as_alice(f"nix-pixie controller pair --code {current} --flake ./infra --machine nope 2>&1"))
        assert "has no machine `nope`; it has: spare, web1" in out, out
        assert code() == current, "the code was used"

    with subtest("inventory, matching by MAC, approval, and the facter report"):
        current = code()
        report = json.loads(controller.succeed(as_alice(f"nix-pixie controller pair --code {current} --flake ./infra --yes")))
        inventory = report["inventory"]

        assert inventory["system"] == "${pkgs.stdenv.hostPlatform.system}", inventory["system"]
        assert inventory["hostname"] == "target", inventory
        assert inventory["memory_bytes"] > 256 * 2**20, inventory
        assert not inventory["firmware"]["uefi"], "test VMs boot without UEFI"
        eth1 = next(n for n in inventory["nics"] if n["name"] == "eth1")
        assert eth1["mac"] == "52:54:00:12:01:02" and eth1["link_up"], eth1
        # Its addresses, the test network's first.
        assert eth1["addrs"][0] == "192.168.1.2", eth1

        disks = {d["name"]: d for d in inventory["disks"]}
        data = next(d for d in disks.values() if "virtio-pixie-data" in d["by_id"])
        assert data["serial"] == "pixie-data", data
        assert data["size_bytes"] == 2**30, data
        assert data["partition_table"] == "gpt", data
        assert any(c["fstype"] == "ext4" and c["label"] == "old-root" for c in data["contents"]), data
        assert not data["mounts"], data
        assert any(d["mounts"] for d in disks.values()), "the root disk is mounted"

        assert report["machine"] == "web1", report
        assert report["configuration"] == "web1", report
        assert report["suggestions"] == [{"machine": "web1", "reasons": ["MAC 52:54:00:12:01:02 (eth1)"]}], report
        target.wait_until_succeeds("journalctl -u nix-pixie-agent | grep -F 'approved by the controller' | grep -F web1")

        assert report["facterReport"] == {"path": "machines/web1/facter.json"}, report
        facter = json.loads(controller.succeed("cat /home/alice/infra/machines/web1/facter.json"))
        assert "hardware" in facter, list(facter)
        status = controller.succeed(as_alice("git -C infra status --porcelain"))
        assert "A  machines/web1/facter.json" in status, status
        next_code(current)

    with subtest("the operator can reject the target"):
        current = code()
        out = controller.fail(as_alice(f"echo n | nix-pixie controller pair --code {current} --flake ./infra 2>&1"))
        assert "Approve this machine?" in out and "HAS DATA: gpt, ext4" in out, out
        assert "rejected" in out, out
        target.wait_until_succeeds("journalctl -u nix-pixie-agent | grep -F 'rejected by the controller'")
        next_code(current)

    with subtest("approval can take a while"):
        current = code()
        # Longer than QUIC's idle timeout: iroh's keep-alives must hold the session.
        report = json.loads(controller.succeed(as_alice(f"(sleep 40; echo y) | nix-pixie controller pair --code {current}")))
        assert report["machine"] is None and report["facterReport"] is None, report
        # The approval was the last message before the controller hung up.
        target.wait_until_succeeds("journalctl -u nix-pixie-agent | grep -F 'approved by the controller' | grep -F 'machine=None'")
        next_code(current)
  '';
}
