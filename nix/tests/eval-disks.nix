# The installed-system disk module (`nix/nixos/disks.nix`), evaluated against
# real disko configurations. The udev rule itself runs in `checks.vm-disko`.
{
  pkgs,
  disko,
  systemModule,
}:
let
  inherit (pkgs) lib;

  eval =
    disk:
    import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = null;
      modules = [
        "${disko}/module.nix"
        systemModule
        {
          nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
          disko.devices.disk.main = disk;
          system.stateVersion = lib.trivial.release;
        }
      ];
    };

  # BIOS GRUB: the layout that keeps using the disk's path after the install.
  gpt = device: {
    inherit device;
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        boot = {
          size = "1M";
          type = "EF02";
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };

  agnostic = eval (gpt "/dev/disk/by-pixie/main");
  pinned = eval (gpt "/dev/disk/by-id/virtio-pixie");
  noTable = eval {
    device = "/dev/disk/by-pixie/main";
    type = "disk";
    content = {
      type = "filesystem";
      format = "ext4";
      mountpoint = "/";
    };
  };

  failedAssertions = c: map (a: a.message) (lib.filter (a: !a.assertion) c.config.assertions);
  hasRules = c: c.config.system.build ? nixPixieDiskRules;
  rules = agnostic.config.system.build.nixPixieDiskRules;
in
assert lib.assertMsg (agnostic.config.boot.loader.grub.devices == [ "/dev/disk/by-pixie/main" ])
  "disko no longer puts the disk's path into grub.devices: ${toString agnostic.config.boot.loader.grub.devices}";
assert lib.assertMsg (hasRules agnostic && failedAssertions agnostic == [ ])
  "agnostic layout: rules expected, no failed assertions; got ${toString (failedAssertions agnostic)}";
assert lib.assertMsg (!hasRules pinned) "a by-id layout needs no rules";
assert lib.assertMsg (lib.any (lib.hasInfix "nix-pixie") (
  failedAssertions noTable
)) "a by-pixie disk without a GPT must fail an assertion";
pkgs.runCommand "eval-disks" { } ''
  rules=${rules}/lib/udev/rules.d/90-nix-pixie-disks.rules
  grep -q 'ENV{DEVTYPE}=="disk", PROGRAM="/nix/store/.*-nix-pixie-disk-role %k", SYMLINK+="disk/by-pixie/%c"' "$rules"
  script=$(grep -o '/nix/store/[^ ]*-nix-pixie-disk-role' "$rules")
  grep -qxF "      disk-main-boot) echo main; exit 0 ;;" "$script"
  grep -qxF "      disk-main-root) echo main; exit 0 ;;" "$script"
  cp "$rules" $out
''
