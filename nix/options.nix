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
          What the controller runs: a package of the flake with the `pixie`
          output (a name or attribute path, e.g. `rekey`), or any installable
          with a `#`. Its main program runs, as with `nix run`. It is
          evaluated and built once, before pairing, so a broken hook doesn't
          cost the pairing code and a hook can't change what others run.

          It runs in the repository, with the operator's terminal;
          `$PIXIE_CONTEXT` names a
          JSON file describing the install so far, and files it writes into
          `$PIXIE_OUT/files/<absolute path>` are put into the installed
          system (with their modes, owned by root). Hooks that change the
          repository must `git add` what the build needs.

          Hooks also provide secrets: `$PIXIE_OUT/disk-keys/<path>` is the
          key disko reads at `<path>` on the target (e.g. a LUKS
          `passwordFile`), from hooks that run before `build`; whatever no
          hook provides is asked for on the terminal. `$PIXIE_OUT/host-key`
          is the host's private key in push mode, from a hook that runs
          before `host-key`.
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

  # A definition with `mkDefault` at every leaf, below its conditions: a
  # `mkDefault` around a `mkIf` would hide the condition from the option.
  # Explicit priorities (`mkForce`, ...) stay. Machine options take no raw
  # attribute sets, so every attribute set here is a group of options.
  weakly =
    value:
    if lib.isAttrs value && !lib.isDerivation value then
      if value._type or null == "if" then
        lib.mkIf value.condition (weakly value.content)
      else if value._type or null == "merge" then
        lib.mkMerge (map weakly value.contents)
      else if value._type or null == "override" then
        value
      else if value ? _type then
        lib.mkDefault value
      else
        lib.mapAttrs (_: weakly) value
    else
      lib.mkDefault value;

  # A module with every definition at `mkDefault` priority, so that a
  # machine's own settings win over `defaults`, option by option.
  weaken =
    module:
    let
      structural = [
        "_class"
        "_file"
        "key"
        "imports"
        "options"
        "disabledModules"
        "freeformType"
        "meta"
      ];
    in
    if lib.isFunction module then
      # Keeps the arguments it asks for (`name`, ...): the module system
      # passes only those.
      lib.setFunctionArgs (args: weaken (module args)) (lib.functionArgs module)
    else if builtins.isPath module || builtins.isString module then
      {
        _file = toString module;
        imports = [ (weaken (import module)) ];
      }
    else
      lib.filterAttrs (name: _: lib.elem name structural) module
      // {
        imports = map weaken (module.imports or [ ]);
        config = weakly (module.config or (removeAttrs module structural));
      };

  # Paths the controller writes into, relative to the flake root. A Nix path
  # literal would be copied to the store (or fail for a file that does not
  # exist yet), and the controller needs the location inside the repository.
  repoRelativePath = types.addCheck types.str (
    p: p != "" && !(lib.hasPrefix "/" p) && !(lib.elem ".." (lib.splitString "/" p))
  );

  machineModule =
    { name, ... }:
    {
      imports = [ (weaken topLevel.defaults) ];

      options = {
        enable = mkOption {
          type = types.bool;
          default = true;
          description = ''
            Whether nix-pixie installs this machine. Every
            `nixosConfiguration` is a machine; `false` leaves out one that
            isn't meant to be installed (a container, an installer image).
          '';
        };

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
          serial = mkOption {
            type = types.listOf types.str;
            default = [ ];
            description = ''
              Serial numbers, from DMI or the device tree (boards without
              DMI), that pre-select this machine. A hint only.
            '';
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

        disks.rules = mkOption {
          type = types.attrsOf (
            types.submodule {
              options = {
                transport = mkOption {
                  type = types.nullOr (types.listOf types.str);
                  default = null;
                  example = [ "nvme" ];
                  description = "Any of these transports, as lsblk names them (`nvme`, `sata`, `usb`, `mmc`, ...).";
                };
                model = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                  description = "The model contains this, in any case.";
                };
                minSize = mkOption {
                  type = types.nullOr types.ints.unsigned;
                  default = null;
                  description = "At least this many bytes.";
                };
                maxSize = mkOption {
                  type = types.nullOr types.ints.unsigned;
                  default = null;
                  description = "At most this many bytes.";
                };
                removable = mkOption {
                  type = types.nullOr types.bool;
                  default = null;
                  description = "Removable, or not.";
                };
              };
            }
          );
          default = { };
          example = lib.literalExpression ''{ main.transport = [ "nvme" ]; }'';
          description = ''
            Which disk a role (a `/dev/disk/by-pixie/<role>` device, or in
            pinned mode a disko disk) takes when nobody chooses: the only
            free disk that matches. More than one, or none, is an error.
            For many devices of one configuration, which can't each have a
            `disks.json`.
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
              `push`: a hook that runs before `host-key` provides the private
              key (`$PIXIE_OUT/host-key`, OpenSSH format, no passphrase); the
              installed system gets it like a pulled one, and the controller
              checks it over SSH after the first boot.
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
              rekeying hook) can use it. With agenix-rekey, read it with
              `age.rekey.hostPubkey = nix-pixie.lib.agenixRekey.hostPubkey ./<this path>;`.
            '';
          };
        };

        facter.reportPath = mkOption {
          type = types.nullOr repoRelativePath;
          default = null;
          example = "machines/web1/facter.json";
          description = ''
            Where the controller writes the nixos-facter report, relative to
            the flake root. `null` disables facter for this machine. `{id}`
            makes it one per device (`controller install --id`).
          '';
        };

        facter.mode = mkOption {
          type = types.enum [
            "write"
            "missing"
            "compare"
          ];
          default = "write";
          description = ''
            What the controller does with the report: `write` it on every
            install; only if it is `missing` from the repository, so devices
            of identical hardware share one; or `compare`: run nixos-facter,
            write nothing, and report where this device's hardware differs
            (written if there is none yet).
          '';
        };

        facter.ignore = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = ''
            Fields `compare` leaves out at any depth, besides those that
            differ between identical devices anyway (serial numbers, MAC
            addresses, UUIDs).
          '';
        };

        bootCheck = mkOption {
          type = types.enum [
            "ssh"
            "hooks"
            "none"
          ];
          default = "ssh";
          description = ''
            How the controller checks that the installed system came up: by
            asking its SSH server for the host key; by the hooks after
            `boot`, which wait for it their own way (a status line on a
            serial port, say) and may report the host key they saw in
            `$PIXIE_OUT/seen-host-key`; or not at all.
          '';
        };

        secrets.recipients = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p" ];
          description = ''
            Age recipients (`age1...`) or SSH public keys that secrets go to
            when a hook hands them to the controller for the repository
            (`$PIXIE_OUT/encrypted/<path>`): they only ever get there
            encrypted, e.g. a LUKS recovery key the target generated.
          '';
        };

        firmware = {
          uefi = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = ''
              Whether the machine boots with UEFI. `null`: as the target
              booted the installer, which is usually how it boots the
              installed system too. An installer booted another way (over
              USB, by a vendor's tool) may not see the firmware the system
              boots with: then say so here, or the controller refuses EFI
              boot loaders.
            '';
          };
          setupMode = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = ''
              Whether the firmware is in Secure Boot setup mode. `null`: as
              the installer sees it. Set it where the installer can't see
              the firmware's variables, and they were cleared beforehand.
            '';
          };
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
      description = ''
        Machines nix-pixie can install. Every `nixosConfiguration` is one,
        named like it; set options here where `defaults` don't fit, or add
        machines that install another configuration.
      '';
    };

    defaults = mkOption {
      type = types.deferredModule;
      default = { };
      example = lib.literalExpression ''
        { name, ... }: {
          hostKey.mode = "pull";
          hostKey.publicKeyPath = "hosts/''${name}/host.pub";
        }
      '';
      description = ''
        Settings for every machine, as a module of machine options that
        gets the machine's `name`. A machine's own settings win over these,
        option by option.
      '';
    };

    hooks = hooksOption "Hooks for every machine, by name.";

    configurations = mkOption {
      type = types.listOf types.str;
      default = [ ];
      internal = true;
      description = "The names of the flake's `nixosConfigurations`: each is a machine.";
    };
  };

  config.machines = lib.genAttrs topLevel.configurations (_: { });
}
