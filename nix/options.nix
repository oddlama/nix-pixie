# The `pixie` option tree. Shared by `lib.mkPixie` (plain Nix) and
# `flakeModules.default` (flake-parts), so both accept the same configuration.
#
# Everything here ends up in the JSON-serialisable `pixie` flake output that
# the controller evaluates. Keep it cheap to evaluate: the controller reads it
# without evaluating any nixosConfiguration.
{ lib, ... }:
let
  inherit (lib) mkOption types;

  macAddress = types.strMatching "([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}";

  # Paths the controller writes into, relative to the flake root. A Nix path
  # literal would be copied to the store (or fail for a file that does not
  # exist yet), and the controller needs the location inside the repository.
  repoRelativePath = types.addCheck types.str (
    p: p != "" && !(lib.hasPrefix "/" p) && !(lib.elem ".." (lib.splitString "/" p))
  );

  machineModule =
    { name, ... }:
    {
      options = {
        configuration = mkOption {
          type = types.str;
          default = name;
          defaultText = lib.literalMD "the machine's attribute name";
          description = "Attribute name in `nixosConfigurations` to install on this machine.";
        };

        match = {
          mac = mkOption {
            type = types.listOf macAddress;
            default = [ ];
            description = ''
              MAC addresses that pre-select this machine for a target.
              A hint only: MAC addresses can be spoofed, so the operator
              still approves every target.
            '';
          };
          dmiSerial = mkOption {
            type = types.listOf types.str;
            default = [ ];
            description = "DMI system serial numbers that pre-select this machine. A hint only.";
          };
        };

        disks.file = mkOption {
          type = types.nullOr repoRelativePath;
          default = null;
          example = "machines/web1/disks.json";
          description = ''
            Pinned mode: where the controller records the disks chosen for
            this machine, relative to the flake root, as
            `{ "<disko disk name>": "/dev/disk/by-id/…" }`. Read it in the
            disko config with `nix-pixie.lib.pinnedDisk`.

            `null`: the controller installs onto the devices the disko config
            names. Devices of the form `/dev/disk/by-pixie/<role>` are chosen
            per install (agnostic mode, one configuration for many machines);
            other paths must exist on the target.
          '';
        };

        hostKey.mode = mkOption {
          type = types.enum [
            "none"
            "push"
            "pull"
          ];
          default = "none";
          description = ''
            `none`: the installed system generates its host key on first boot.
            `push`: the controller provides the host key.
            `pull`: the target generates the key during install, and hooks
            (e.g. rekeying) run before the system is built.
          '';
        };

        facter.reportPath = mkOption {
          type = types.nullOr repoRelativePath;
          default = null;
          example = "machines/web1/facter.json";
          description = ''
            Where the controller writes the nixos-facter report, relative to
            the flake root. `null` disables facter for this machine.
          '';
        };
      };
    };
in
{
  options.machines = mkOption {
    type = types.attrsOf (types.submodule machineModule);
    default = { };
    description = "Machines nix-pixie can install.";
  };
}
