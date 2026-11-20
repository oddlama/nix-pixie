# Keeps `/dev/disk/by-pixie/<role>` working on the installed system.
#
# In agnostic mode the disko config names its disks `/dev/disk/by-pixie/<role>`
# and the agent links the disks the operator chose there for the install. Some
# options keep using that path afterwards. Most importantly, disko puts the
# disk of a BIOS boot partition (EF02) into `boot.loader.grub.devices`, so
# every rebuild runs `grub-install /dev/disk/by-pixie/<role>`.
#
# This recreates the links on every boot: a udev rule recognises each disk by
# the GPT partition labels disko gave it. Disks without a GPT have no labels
# and can't be recognised (an assertion says so). Two disks carrying the same
# labels (e.g. one moved over from another machine with the same config) both
# claim the link, and udev picks one.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  prefix = "/dev/disk/by-pixie/";

  # Nothing to do without disko.
  disks = config.disko.devices.disk or { };

  # Unset devices are fine here; disko reports them itself.
  device =
    disk:
    let
      d = builtins.tryEval disk.device;
    in
    if d.success then d.value else null;

  gptLabels =
    disk:
    if (disk.content.type or null) == "gpt" then
      map (partition: partition.label) (lib.attrValues disk.content.partitions)
    else
      [ ];

  byRole = lib.filterAttrs (_: disk: lib.hasPrefix prefix (toString (device disk))) disks;

  roles = lib.mapAttrs (_: disk: {
    role = lib.removePrefix prefix (device disk);
    labels = gptLabels disk;
  }) byRole;

  cases = lib.concatLists (
    lib.mapAttrsToList (
      _:
      { role, labels }:
      map (label: "${lib.escapeShellArg label}) echo ${lib.escapeShellArg role}; exit 0 ;;") labels
    ) roles
  );

  # Prints the role of the disk named $1, recognised by its partitions' GPT
  # labels (the kernel's PARTNAME).
  roleOf = pkgs.writeShellScript "nix-pixie-disk-role" ''
    for uevent in /sys/class/block/"$1"/*/uevent; do
      while IFS== read -r key value; do
        [ "$key" = PARTNAME ] || continue
        case "$value" in
          ${lib.concatStringsSep "\n      " cases}
        esac
      done < "$uevent"
    done
    exit 1
  '';

  rules = pkgs.writeTextDir "lib/udev/rules.d/90-nix-pixie-disks.rules" ''
    ACTION!="remove", SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", PROGRAM="${roleOf} %k", SYMLINK+="disk/by-pixie/%c"
  '';
in
{
  config = lib.mkIf (roles != { }) {
    assertions = lib.mapAttrsToList (name: r: {
      assertion = builtins.match "[A-Za-z0-9][A-Za-z0-9._-]*" r.role != null && r.labels != [ ];
      message = ''
        nix-pixie: disko.devices.disk.${name} uses ${prefix}${r.role}, which only
        works after the install if the role is a plain name ([A-Za-z0-9._-])
        and the disk has a GPT with partitions (their labels identify it).
      '';
    }) roles;

    services.udev.packages = [ rules ];
    # For tests: the rules on their own.
    system.build.nixPixieDiskRules = rules;
  };
}
