# The layouts `checks.vm-install-<name>` install and boot, from `targets/`.
#
# - disks: the disko disks, in the order they get the target's empty disks.
# - zfs: the installer needs ZFS (our ISO has it).
# - hostKeyPath: where the configuration's ed25519 host key lives.
# - passphrase: typed in on the controller for each key file, and at boot.
# - bootPrompt: how the installed system asks for it on its console.
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
