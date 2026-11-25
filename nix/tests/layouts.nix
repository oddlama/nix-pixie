# The layouts `checks.vm-install-<name>` install and boot, from `targets/`.
#
# - disks: the disko disks, in the order they get the target's empty disks.
# - zfs: the installer needs ZFS (our ISO has it).
# - hostKeyPath: where the configuration's ed25519 host key lives.
# - hooks, packages, builtModules: the machine's hooks (Nix source), the file
#   in `targets/` with the packages they run, and what they change in the
#   repository before the build (see `user-flake.nix`).
# - target: the file in `targets/` (default: `<name>.nix`).
# - hostKeyMode: the machine's `hostKey.mode` (default: pull).
# - passphrase: typed in on the controller for each key file (unless
#   `typed = false`: then hooks provide them), and at boot.
# - bootPrompt: how the installed system asks for it on its console.
# - live: the controller runs on a terminal (with TERM set), so its console
#   is live, and the first boot is checked once the test presses Enter.
# - retry: on a live console, a question to answer with Enter when it shows.
# - checks: Python run against the booted `installed` machine.
{
  ext4 = {
    disks = [ "main" ];
    checks = ''
      source = installed.succeed("findmnt -n -o SOURCE /").strip()
      assert source == "/dev/vda2", source
    '';
  };

  zfs = {
    disks = [ "main" ];
    zfs = true;
    checks = ''
      fstype = installed.succeed("findmnt -n -o FSTYPE /").strip()
      assert fstype == "zfs", fstype
      nix = installed.succeed("findmnt -n -o SOURCE /nix").strip()
      assert nix == "zroot/nix", nix
      # Imported although forceImportRoot is off: the installer exported it.
      status = installed.succeed("zpool status -x zroot")
      assert "is healthy" in status, status
    '';
  };

  btrfs = {
    disks = [ "main" ];
    checks = ''
      fstype = installed.succeed("findmnt -n -o FSTYPE /").strip()
      assert fstype == "btrfs", fstype
      root = installed.succeed("findmnt -n -o OPTIONS /")
      assert "subvol=/root" in root, root
      # Separate subvolumes. (Their mount options can't be checked: btrfs
      # applies the first mounted subvolume's to the whole filesystem.)
      nix = installed.succeed("findmnt -n -o OPTIONS /nix")
      assert "subvol=/nix" in nix, nix
    '';
  };

  lvm = {
    disks = [ "main" ];
    checks = ''
      source = installed.succeed("findmnt -n -o SOURCE /").strip()
      assert source == "/dev/mapper/pool-root", source
    '';
  };

  mdadm = {
    disks = [
      "a"
      "b"
    ];
    checks = ''
      root = installed.succeed("findmnt -n -o SOURCE /").strip()
      assert installed.succeed(f"readlink -f {root}").strip().startswith("/dev/md"), root
      detail = installed.succeed("mdadm --detail /dev/md/root")
      assert "Active Devices : 2" in detail, detail
    '';
  };

  luks = {
    disks = [ "main" ];
    passphrase = "secretsecret";
    bootPrompt = "[Pp]assphrase for";
    checks = ''
      source = installed.succeed("findmnt -n -o SOURCE /").strip()
      assert source == "/dev/mapper/cryptroot", source
      installed.succeed("cryptsetup isLuks /dev/vda3")
    '';
  };

  # LUKS without anyone at the controller: the passphrase and the host key
  # (push mode) come from hooks, and nothing gives the controller a terminal.
  luks-scripted = {
    target = "luks.nix";
    disks = [ "main" ];
    hostKeyMode = "push";
    packages = "hook-packages.nix";
    hooks = ''
      {
        push-key = { command = "push-key"; before = [ "host-key" ]; };
        luks-key = { command = "luks-key"; after = [ "disks" ]; };
      }
    '';
    typed = false;
    passphrase = "secretsecret";
    bootPrompt = "[Pp]assphrase for";
    checks = ''
      # The pushed key, not one the target generated.
      pushed = ${builtins.toJSON (builtins.readFile ./targets/pushed-host-key.pub)}.split()[:2]
      installed_key = installed.succeed("cat /etc/ssh/ssh_host_ed25519_key.pub").split()[:2]
      assert installed_key == pushed, installed_key
      source = installed.succeed("findmnt -n -o SOURCE /").strip()
      assert source == "/dev/mapper/cryptroot", source
    '';
  };

  zfs-encrypted = {
    disks = [ "main" ];
    zfs = true;
    passphrase = "secretsecret";
    bootPrompt = "[Kk]ey for zroot/root";
    checks = ''
      for prop, want in [("encryption", "aes-256-gcm"), ("keylocation", "prompt"), ("keystatus", "available")]:
          got = installed.succeed(f"zfs get -H -o value {prop} zroot/root").strip()
          assert got == want, f"{prop}: {got}"
    '';
  };

  # Also the live console: hooks get the terminal in the middle of it.
  hooks = {
    disks = [ "main" ];
    live = true;
    retry = "Run hook flaky again";
    packages = "hook-packages.nix";
    hooks = ''
      {
        stamp = { command = "stamp"; after = [ "host-key" ]; before = [ "build" ]; };
        flaky = { command = "flaky"; after = [ "approve" ]; };
        secret = { command = "secret"; after = [ "disks" ]; };
        booted = { command = "booted"; after = [ "boot" ]; };
      }
    '';
    builtModules = [ { environment.etc."pixie-stamp".text = "stamped before the build\n"; } ];
    checks = ''
      # Changed the repository before the build, which picked it up.
      stamp = installed.succeed("cat /etc/pixie-stamp")
      assert stamp == "stamped before the build\n", stamp
      staged = controller.succeed("su - alice -c 'cd repo && git diff --cached --name-only'")
      assert "machines/web1/stamp" in staged, staged
      # Written before the disks were formatted, placed once they were.
      secret = installed.succeed("cat /var/lib/hook/secret")
      assert secret == "from the hook", secret
      mode = installed.succeed("stat -c '%a %U' /var/lib/hook/secret").strip()
      assert mode == "640 root", mode
      # Each saw what was known by its step.
      seen = {n: json.loads(controller.succeed(f"cat /tmp/context-{n}.json")) for n in ["stamp", "secret", "booted"]}
      assert seen["stamp"]["step"] == "host-key", seen["stamp"]
      assert seen["stamp"]["hostKey"].startswith("ssh-ed25519 ") and seen["stamp"]["system"] is None, seen["stamp"]
      assert seen["secret"]["disks"][0]["disk"] == "vdb" and seen["secret"]["root"] is None, seen["secret"]
      assert seen["booted"]["step"] == "boot" and seen["booted"]["system"].startswith("/nix/store/"), seen["booted"]
      ran = {h["name"]: (h["after"], h["ran"]) for h in report["hooks"]}
      assert ran == {"flaky": ("approve", True), "stamp": ("host-key", True), "secret": ("disks", True), "booted": ("boot", True)}, ran
      # It failed once, and ran again after the question.
      assert "flaky: failing on purpose" in output, output
      # The live console left the steps as the last word, all done.
      shown = without_escapes(output)
      steps = shown[shown.rindex("Check the flake"):]
      for step in ["Run hook flaky", "Run hook stamp", "Build nixosConfigurations.web1", "Run hook booted", "Wait for the first boot"]:
          assert re.search(r"▣ " + re.escape(step), steps), steps
      # Details are cut at the terminal's width (80 columns here).
      assert "✗" not in steps and "up at 192.168.1.3, host key" in steps, steps
      assert "checking whether web1 is up" in shown, shown
    '';
  };

  tmpfs-root = {
    disks = [ "main" ];
    hostKeyPath = "/nix/persist/ssh_host_ed25519_key";
    checks = ''
      root = installed.succeed("findmnt -n -o FSTYPE /").strip()
      assert root == "tmpfs", root
      nix = installed.succeed("findmnt -n -o FSTYPE /nix").strip()
      assert nix == "ext4", nix
      # Nothing at the default path: sshd uses the key from hostKeys.
      installed.fail("test -e /etc/ssh/ssh_host_ed25519_key")
    '';
  };
}
