# A target on another network that nobody can connect to: no open ports, and
# at first not even a route from the controller. All the two share is a
# relay server both reach, which is where the controller finds the target
# with nothing but its code.
#
# First there is no route between the two networks, so everything goes
# through the relay, a whole install included. The controller can't look for
# the installed system afterwards, and says so. Then the machine in the
# middle starts routing: the target still lets nothing in, but both now send
# to each other, which opens their firewalls for the answers, and the
# install's data takes that direct path. It ends at the target itself, so
# that is where the controller checks the installed system's host key.
#
# The relay is nixpkgs' `iroh-relay` in its development mode (plain HTTP),
# standing in for the public ones the agent and the controller use unless
# told otherwise. Those are unreachable here, like on any isolated network.
#
# Nodes are numbered alphabetically: controller 1, installed 2, relay 3,
# target 4. The installed system takes over the target's address
# (`targets/relay.nix`).
{
  pkgs,
  package,
  agentModule,
  nixpkgs,
  disko,
}:
let
  relayUrl = "http://192.168.1.3:3340";
  targetAddress = "192.168.2.4";

  flake = import ./user-flake.nix { inherit pkgs nixpkgs disko; } {
    target = "relay.nix";
    configurations.web1.disks.main = {
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
  name = "nix-pixie-relay";

  nodes = {
    # The default firewall with nothing opened, like an operator's laptop.
    controller = {
      imports = [ flake.controllerModule ];
      environment.systemPackages = [ package ];
      networking.interfaces.eth1.ipv4.routes = [
        {
          address = "192.168.2.0";
          prefixLength = 24;
          via = "192.168.1.3";
        }
      ];
    };

    # Boots the target's disk with its own boot loader. Its own
    # configuration only matters for QEMU's command line.
    installed = {
      virtualisation.vlans = [ 2 ];
      virtualisation.useBootLoader = true;
      virtualisation.useDefaultFilesystems = false;
      virtualisation.diskImage = "./empty0.qcow2";
      virtualisation.fileSystems."/" = {
        device = "/dev/disk/by-label/unused";
        fsType = "ext4";
      };
    };

    # On both networks, but not a router until the test makes it one.
    relay = {
      virtualisation.vlans = [
        1
        2
      ];
      systemd.services.iroh-relay = {
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        serviceConfig = {
          ExecStart = "${pkgs.lib.getExe pkgs.iroh-relay} --dev";
          DynamicUser = true;
        };
      };
      networking.firewall.allowedTCPPorts = [ 3340 ];
    };

    target = {
      imports = [
        agentModule
        ../nixos/storage.nix
      ];
      virtualisation.vlans = [ 2 ];
      networking.interfaces.eth1.ipv4.routes = [
        {
          address = "192.168.1.0";
          prefixLength = 24;
          via = "192.168.2.3";
        }
      ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
        openFirewall = false;
        relay.urls = [ relayUrl ];
      };
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

    relay_url = "${relayUrl}"
    target_address = "${targetAddress}"
    install = (
        f"nix-pixie controller install --relay {relay_url} --machine web1 --disk main=vdb --yes"
        + " --then poweroff --boot-timeout 300 --boot-check-interval 5"
    )

    def as_alice(cmd):
        return f"su - alice -c '{cmd}'"

    def next_code(old):
        target.wait_until_succeeds(
            f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != '{old}'"
            # The code comes first; the rendezvous is ready a moment later.
            + " && test -s /run/nix-pixie/relay"
        )
        return target.succeed("cat /run/nix-pixie/code").strip()

    controller.start()
    relay.start()
    target.start()
    relay.wait_for_open_port(3340)
    target.wait_for_unit("nix-pixie-agent.service")
    controller.wait_for_unit("multi-user.target")
    ${flake.setupScript}

    with subtest("nothing gets to the target, from anywhere"):
        controller.succeed("ping -c1 -W2 192.168.1.3")
        controller.fail(f"ping -c1 -W2 {target_address}")
        target.fail("ping -c1 -W2 192.168.1.1")
        rules = target.succeed("iptables -S nixos-fw")
        assert "4701" not in rules and "5353" not in rules, rules

    with subtest("the target waits at the relay and says how to find it"):
        code = next_code("")
        assert len(code.split("-")[0]) == 6, code
        assert target.succeed("cat /run/nix-pixie/relay").strip() == relay_url + "/"
        target.succeed(
            "journalctl -u nix-pixie-agent -o cat"
            f" | grep -F -- 'nix-pixie controller pair --code {code} --relay {relay_url}/'"
        )

    with subtest("without the relay the controller finds nobody, and says where it looked"):
        out = controller.fail(as_alice(f"nix-pixie controller pair --code {code} --no-relay --timeout 5") + " 2>&1")
        assert "no target with nameplate" in out and "only found with --target" in out, out
        # The public relays, which the VM doesn't reach.
        out = controller.fail(as_alice(f"nix-pixie controller pair --code {code} --timeout 8") + " 2>&1")
        assert "this machine reached none of n0's public relays" in out, out
        # A relay it reaches, where nobody shows this code.
        typo = f"{int(code.split('-')[0]) ^ 1}-{code.split('-', 1)[1]}"
        out = controller.fail(as_alice(f"nix-pixie controller pair --code {typo} --relay {relay_url} --timeout 8") + " 2>&1")
        assert f"no target waiting at {relay_url}/" in out, out

    with subtest("the code and the relay are enough to pair"):
        report = json.loads(controller.succeed(
            as_alice(f"nix-pixie controller pair --code {code} --relay {relay_url} --yes 2>/tmp/pair.err")
        ))
        err = controller.succeed("cat /tmp/pair.err")
        print(err)
        assert report["label"] == "target", report
        # Nothing but the relay connects them.
        assert report["route"] == "relay" and report["addrs"] == [], report
        assert f"Paired with target (relayed through {relay_url}/)" in err, err
        code = next_code(code)

    with subtest("a whole install goes through the relay, but nobody can look for the result"):
        report = json.loads(controller.succeed(
            as_alice(f"cd repo && {install} --code {code} 2>/tmp/install.err")
        ))
        err = controller.succeed("cat /tmp/install.err")
        print(err)
        assert report["disks"][0]["disk"] == "vdb", report
        assert report["route"] == "relay", report
        assert "copying through the relay" in err and f"relay={relay_url}/" in err, err
        # No address to ask for the host key: said, not waited for.
        recorded = controller.succeed("cat ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        host_key = report["hostKey"]
        assert "only reached through a relay" in host_key["skipped"], host_key
        assert host_key["expected"] == " ".join(recorded.split()[:2]), host_key
        assert "Not waiting for web1 to boot" in err and host_key["expected"] in err, err
        target.wait_for_shutdown()

    with subtest("with a route between them, the install moves to a direct path"):
        relay.succeed("sysctl -w net.ipv4.ip_forward=1")
        # The installer again: the test VM boots its own kernel, whatever is
        # on the disk it installed onto.
        target.start()
        target.wait_for_unit("nix-pixie-agent.service")
        code = next_code("")
        controller.wait_until_succeeds(f"ping -c1 -W2 {target_address}")
        # In the background: after the install, it waits for the system to
        # boot. The disk holds the first install now, hence --wipe.
        controller.succeed(
            "systemd-run --unit=pixie-install --remain-after-exit --uid=alice"
            + " --working-directory=/home/alice/repo -E HOME=/home/alice"
            + " -E PATH=/run/current-system/sw/bin"
            + " -p StandardOutput=file:/tmp/install.out -p StandardError=inherit"
            + f" {install} --wipe --code {code}"
        )

        def wait_for_output(text):
            # If the install fails, stop at once instead of waiting for the
            # test's timeout.
            controller.wait_until_succeeds(
                f"grep -q '{text}' /tmp/install.out"
                + " || test \"$(systemctl show -P Result pixie-install)\" != success",
                timeout=600,
            )
            result = controller.succeed("systemctl show -P Result pixie-install").strip()
            output = controller.succeed("cat /tmp/install.out")
            assert result == "success", f"the install failed ({result}):\n{output}"

        wait_for_output("Send the system")
        # The target's firewall never opened anything.
        assert target.succeed("iptables -S nixos-fw") == rules
        wait_for_output("is powered off")
        target.wait_for_shutdown()

    with subtest("the direct path ends at the target itself, so its host key is checked there"):
        installed.state_dir = target.state_dir
        installed.start()
        installed.wait_for_unit("multi-user.target", timeout=300)
        controller.wait_until_succeeds(
            "systemctl show -P SubState pixie-install | grep -qx -e exited -e failed",
            timeout=300,
        )
        result = controller.succeed("systemctl show -P Result pixie-install").strip()
        output = controller.succeed("cat /tmp/install.out")
        print(output)
        assert result == "success", result
        assert "copying through the relay" not in output, output
        report = json.loads(next(l for l in reversed(output.splitlines()) if l.lstrip().startswith("{")))
        assert report["route"] == "direct", report
        recorded = controller.succeed("cat ~alice/repo/machines/web1/ssh_host_ed25519_key.pub")
        host_key = report["hostKey"]
        assert host_key["verified"] and host_key["seen"] == " ".join(recorded.split()[:2]), host_key
        assert host_key["address"] == target_address, host_key
        public = installed.succeed("cat /etc/ssh/ssh_host_ed25519_key.pub")
        assert " ".join(public.split()[:2]) == host_key["seen"], public
  '';
}
