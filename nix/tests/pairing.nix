# Discovery and pairing between a controller and installer agents on an
# isolated network (test VMs have no internet, so this is also the airgapped
# case: the relays both sides would use by default are out of reach).
# Covers both pairing modes, wrong codes, code rotation, each discovery
# method, agents with and without relays, and what an agent costs while its
# relays are out of reach.
#
# The controller keeps the default NixOS firewall with nothing opened, like an
# operator's laptop.
{
  pkgs,
  package,
  agentModule,
}:
let
  exe = pkgs.lib.getExe package;
in
pkgs.testers.runNixOSTest {
  name = "nix-pixie-pairing";

  nodes = {
    controller = {
      environment.systemPackages = [ package ];
      users.users.alice.isNormalUser = true;
    };

    # Default mode: the agent shows a code.
    target = {
      imports = [ agentModule ];
      services.nix-pixie-agent = {
        enable = true;
        inherit package;
      };
    };

    # Controller-code mode: the agent is started by hand with a typed code.
    target2 = {
      environment.systemPackages = [ package ];
      networking.firewall.allowedUDPPorts = [
        47011
        47012
        5353
      ];
    };
  };

  testScript = ''
    import json
    import re
    import time

    def as_alice(cmd):
        return f"su - alice -c '{cmd}'"

    def current_code():
        target.wait_for_file("/run/nix-pixie/code")
        return target.succeed("cat /run/nix-pixie/code").strip()

    def next_code(old):
        target.wait_until_succeeds(f"test -s /run/nix-pixie/code && test \"$(cat /run/nix-pixie/code)\" != {old}")
        return current_code()

    def pair(code, discovery="both"):
        out = controller.succeed(as_alice(f"nix-pixie controller pair --yes --discovery {discovery} --code {code}"))
        report = json.loads(out)
        assert report["target"] == target_id, report
        # Discovery must find the routable address, not just link-local.
        assert "192.168.1.2:47011" in report["addrs"], report
        return report

    def wrong_code(code):
        nameplate = code.split("-")[0]
        wrong = f"{nameplate}-acid-acorn-acre"
        return wrong if wrong != code else f"{nameplate}-acid-acorn-aging"

    start_all()
    target.wait_for_unit("nix-pixie-agent.service")
    # The unit is active before the agent has bound its endpoint.
    target.wait_for_file("/run/nix-pixie/endpoint-id")
    target_id = target.succeed("cat /run/nix-pixie/endpoint-id").strip()
    controller_id = controller.succeed(as_alice("nix-pixie controller id")).strip()

    with subtest("identity is private and stable"):
        controller.succeed("test $(stat -c %a /home/alice/.local/state/nix-pixie/identity) = 600")
        assert controller.succeed(as_alice("nix-pixie controller id")).strip() == controller_id

    with subtest("the code is on the console"):
        code = current_code()
        # Long enough to be found at a relay, even though none is in reach.
        assert len(code.split("-")[0]) == 6, code
        target.wait_until_succeeds(f"journalctl -u nix-pixie-agent -o cat | grep -F 'pairing code:  {code}'")

    with subtest("controller pairs through its closed firewall"):
        report = pair(code)
        assert report["label"] == "target", report
        target.wait_until_succeeds(
            f"journalctl -u nix-pixie-agent | grep 'paired with controller' | grep -F {controller_id}"
        )
        code = next_code(code)

    with subtest("probes alone find the target"):
        pair(code, discovery="probe")
        code = next_code(code)

    with subtest("mDNS alone is blocked by the firewall, and the error says so"):
        out = controller.fail(as_alice(f"nix-pixie controller pair --yes --discovery mdns --timeout 5 --code {code} 2>&1"))
        assert "drops mDNS replies" in out, out

    with subtest("mDNS alone works once the firewall lets it in"):
        controller.succeed("nixos-firewall-tool open udp 5353")
        pair(code, discovery="mdns")
        controller.succeed("nixos-firewall-tool reset")
        code = next_code(code)

    with subtest("wrong codes fail on both sides, then the code rotates"):
        for attempt in range(3):
            out = controller.fail(as_alice(f"nix-pixie controller pair --yes --timeout 5 --code {wrong_code(code)} 2>&1"))
            assert "accepted the code" in out, out
        target.wait_until_succeeds("journalctl -u nix-pixie-agent | grep -F 'someone may be guessing'")
        code = next_code(code)

    with subtest("the rotated code pairs"):
        pair(code)
        code = next_code(code)

    with subtest("a target whose clock is days off gets the controller's, real-time clock too"):
        target.succeed("date -s @$(( $(date +%s) - 3 * 86400 ))")
        pair(code)
        code = next_code(code)
        target.succeed("journalctl -u nix-pixie-agent | grep -q 'The clock was 3 days behind'")
        target.fail("journalctl -u nix-pixie-agent | grep -q 'setting the real-time clock'")
        off = abs(int(target.succeed("date +%s")) - int(controller.succeed("date +%s")))
        assert off <= 5, f"the clocks are {off} s apart"
        rtc = abs(int(target.succeed("date -d \"$(hwclock --get)\" +%s")) - int(target.succeed("date +%s")))
        assert rtc <= 5, f"the real-time clock is {rtc} s off"

    with subtest("without --code the controller asks for it, completing words with Tab"):
        words = [w.strip() for w in open("${../../crates/nix-pixie-proto/src/wordlist.txt}") if w.strip() and not w.startswith("#")]

        def shortest(word):
            # As few letters as name only this word.
            return next(word[:n] for n in range(1, len(word) + 1) if [w for w in words if w.startswith(word[:n])] == [word])

        nameplate, first, second, third = code.split("-")
        # For printf: Tab, Enter (\\r) and Backspace (\\177). The nameplate
        # completes from the targets found meanwhile.
        keys = (
            f"{nameplate[:3]}\\t{shortest(first)}\\t{shortest(second)}\\t"
            # A typo: Enter refuses it, and it is taken back.
            + "zzz\\r\\177\\177\\177"
            + f"{shortest(third)}\\t\\r"
        )
        # Typed once the prompt is there, into a pipe held open for it.
        controller.succeed("mkfifo -m 666 /tmp/keys && systemd-run --unit=hold-keys -E PATH=$PATH sh -c 'exec sleep infinity > /tmp/keys'")
        controller.succeed(
            "systemd-run --unit=typed-pair --uid=alice -E HOME=/home/alice -E PATH=/run/current-system/sw/bin"
            + " -E TERM=xterm -p StandardInput=file:/tmp/keys -p StandardOutput=file:/tmp/typed.out"
            + " script --quiet --return --flush --command 'nix-pixie controller pair --yes' /dev/null"
        )
        controller.wait_until_succeeds("grep -q 'Code shown on the target' /tmp/typed.out", timeout=60)
        time.sleep(5)
        controller.succeed(f"printf '{keys}' 1<>/tmp/keys")
        try:
            controller.wait_until_succeeds("systemctl show -P SubState typed-pair | grep -qx -e exited -e failed -e dead", timeout=60)
        finally:
            out = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", controller.succeed("cat /tmp/typed.out"))
            print(out)
        assert controller.succeed("systemctl show -P Result typed-pair").strip() == "success", out
        assert "`zzz` is not a code word" in out, out
        report = json.loads(next(l for l in reversed(out.splitlines()) if l.lstrip().startswith("{")))
        assert report["target"] == target_id, report
        controller.succeed("systemctl stop hold-keys")
        code = next_code(code)

    with subtest("a typo in the nameplate finds nobody, and the error says where it looked"):
        nameplate = int(code.split("-")[0])
        other = f"{nameplate ^ 1}-{code.split('-', 1)[1]}"
        out = controller.fail(as_alice(f"nix-pixie controller pair --yes --timeout 5 --code {other} 2>&1"))
        assert "found" in out and "eth1 192.168.1.1/24" in out, out
        assert "reached none of n0's public relays" in out, out

    with subtest("target types the controller's code; the controller dials it"):
        controller.succeed(
            "systemd-run --unit=pixie-serve --uid=alice --setenv=HOME=/home/alice"
            # stderr would otherwise inherit stdout's file.
            " -p StandardOutput=file:/home/alice/serve.json -p StandardError=journal"
            " ${exe} controller serve --code-file /home/alice/code --max-targets 1"
        )
        controller.wait_for_file("/home/alice/code")
        controller_code = controller.succeed("cat /home/alice/code").strip()
        target2.succeed(
            "systemd-run --unit=pixie-agent2"
            f" ${exe} agent --state-dir /run/nix-pixie2 --controller-code {controller_code}"
        )
        target2.wait_for_file("/run/nix-pixie2/endpoint-id")
        target2_id = target2.succeed("cat /run/nix-pixie2/endpoint-id").strip()
        controller.wait_until_succeeds("grep -q '\"target\"' /home/alice/serve.json")
        report = json.loads(controller.succeed("cat /home/alice/serve.json"))
        assert report["target"] == target2_id, report
        assert report["label"] == "target2", report
        target2.wait_until_succeeds(
            f"journalctl -u pixie-agent2 | grep 'paired with controller' | grep -F {controller_id}"
        )
        # serve exits after one target; the agent then ends cleanly.
        controller.wait_until_fails("systemctl is-active pixie-serve")
        target2.wait_until_succeeds("journalctl -u pixie-agent2 | grep -F 'controller disconnected'")

    with subtest("an agent without relays shows a short code, and pairs"):
        target2.wait_until_fails("systemctl is-active pixie-agent2")
        target2.succeed(
            "systemd-run --unit=pixie-agent3 ${exe} agent --state-dir /run/nix-pixie3 --no-relay"
        )
        target2.wait_for_file("/run/nix-pixie3/code")
        short = target2.succeed("cat /run/nix-pixie3/code").strip()
        assert 1 <= int(short.split("-")[0]) <= 99, short
        out = controller.succeed(as_alice(f"nix-pixie controller pair --yes --code {short}"))
        assert json.loads(out)["label"] == "target2", out
        target2.succeed("systemctl stop pixie-agent3")

    with subtest("an agent that reaches no relay doesn't pile up sockets"):
        # Looking up the relays' names where no name server answers once held
        # hundreds of sockets at a time: enough to run out of open files, so
        # that a pairing just then failed.
        target2.succeed("systemd-run --unit=pixie-agent4 ${exe} agent --state-dir /run/nix-pixie4")
        target2.wait_for_file("/run/nix-pixie4/code")
        most = int(target2.succeed(
            "pid=$(systemctl show -p MainPID --value pixie-agent4); most=0;"
            " for _ in $(seq 60); do"
            " n=$(ls /proc/$pid/fd | wc -l); [ $n -gt $most ] && most=$n; sleep 0.2;"
            " done; echo $most"
        ))
        assert 0 < most < 64, most
        target2.succeed("systemctl stop pixie-agent4")

    with subtest("the controller never listened"):
        # No sockets bound to the agent ports on the operator's machine.
        controller.fail("ss -ulnp | grep -E ':4701[12] '")
  '';
}
