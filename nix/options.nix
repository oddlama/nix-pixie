# The `pixie` option tree. Shared by `lib.mkPixie` (plain Nix) and
# `flakeModules.default` (flake-parts), so both accept the same configuration.
#
# Everything here ends up in the JSON-serialisable `pixie` flake output that
# the controller evaluates. Keep it cheap to evaluate: the controller reads it
# without evaluating any nixosConfiguration.
{ config, lib, ... }:
let
  inherit (lib) mkOption types;
  topLevel = config;

  # The install's own steps, in order. Hooks are ordered against them (and
  # each other); the controller checks the names (`crates/nix-pixie/src/hooks.rs`).
  steps = "`approve`, `facter`, `host-key`, `disks`, `build`, `format`, `copy`, `install`, `finish`, `boot`";

  hookModule = {
    options = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Whether the hook runs. `false` drops a hook every machine has for one machine.";
      };
      command = mkOption {
        type = types.str;
        example = "agenix-rekey";
        description = ''
          What the controller runs, with `nix run`: an app or package of the
          flake with the `pixie` output (a name or attribute path, e.g.
          `rekey`), or any installable with a `#`. Built before pairing, so a
          broken hook doesn't cost the pairing code. It runs in the
          repository, with the operator's terminal; `$PIXIE_CONTEXT` names a
          JSON file describing the install so far, and files it writes into
          `$PIXIE_OUT/files/<absolute path>` are put into the installed
          system (with their modes, owned by root). Hooks that change the
          repository must `git add` what the build needs.
        '';
      };
      args = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Arguments for the command.";
      };
      after = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "host-key" ];
        description = ''
          Steps of the install, or other hooks, that must be done before
          this hook runs. Steps: ${steps}. Hooks always run after `approve`,
          and as early as their constraints allow.
        '';
      };
      before = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "build" ];
        description = "Steps or hooks that must wait for this hook. Steps: ${steps}.";
      };
    };
  };
  hooksOption =
    description:
    mkOption {
      type = types.attrsOf (types.submodule hookModule);
      default = { };
      inherit description;
    };

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

        hostKey = {
          mode = mkOption {
            type = types.enum [
              "none"
              "push"
              "pull"
            ];
            default = "none";
            description = ''
              `none`: the installed system generates its host key on first
              boot; the controller can only report the key it sees.
              `push`: the controller provides the host key (not implemented
              yet; the controller refuses it).
              `pull`: the target generates an ed25519 key right after
              approval, before anything is built, and the installed system
              gets it at the path its `services.openssh.hostKeys` names. The
              controller checks it over SSH after the first boot.
            '';
          };
          publicKeyPath = mkOption {
            type = types.nullOr repoRelativePath;
            default = null;
            example = "machines/web1/ssh_host_ed25519_key.pub";
            description = ''
              Pull mode: where the controller records the public key, relative
              to the flake root, before building, so the configuration (or a
              rekeying hook) can use it.
            '';
          };
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

        hooks = hooksOption ''
          This machine's hooks, on top of `hooks`. Settings here win over the
          ones from `hooks`, option by option.
        '';
      };

      # Every machine gets the hooks for every machine, as defaults.
      config.hooks = lib.mapAttrs (
        _: hook:
        lib.mapAttrs (_: lib.mkDefault) {
          inherit (hook)
            enable
            command
            args
            after
            before
            ;
        }
      ) topLevel.hooks;
    };
in
{
  options = {
    machines = mkOption {
      type = types.attrsOf (types.submodule machineModule);
      default = { };
      description = "Machines nix-pixie can install.";
    };

    hooks = hooksOption "Hooks for every machine, by name.";
  };
}
