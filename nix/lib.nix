# Plain-Nix interface. Usable without flake-parts:
#
#   outputs = { self, nixpkgs, nix-pixie, ... }: {
#     nixosConfigurations.web1 = nixpkgs.lib.nixosSystem { ... };
#     pixie = nix-pixie.lib.mkPixie {
#       inherit (self) nixosConfigurations;
#       config.machines.web1.match.mac = [ "aa:bb:cc:dd:ee:ff" ];
#     };
#   };
{ lib }:
let
  # Bump when the shape of the metadata changes; the controller checks it.
  metadataVersion = 1;

  optionsModule = ./options.nix;

  # Turn an evaluated `pixie` config into the JSON-serialisable flake output.
  # Takes the attribute names of `nixosConfigurations` only, so it never forces
  # a NixOS evaluation.
  mkMetadata =
    { config, nixosConfigurations }:
    let
      known = builtins.attrNames nixosConfigurations;
      checkMachine =
        name: machine:
        if lib.elem machine.configuration known then
          machine
        else
          throw "nix-pixie: machine `${name}` refers to nixosConfigurations.${machine.configuration}, which does not exist. Known: ${lib.concatStringsSep ", " known}";
      normalise =
        machine:
        machine
        // {
          match = machine.match // {
            mac = map lib.toLower machine.match.mac;
          };
          # Only the enabled ones, without the switch.
          hooks = lib.mapAttrs (_: hook: removeAttrs hook [ "enable" ]) (
            lib.filterAttrs (_: hook: hook.enable) machine.hooks
          );
        };
    in
    {
      version = metadataVersion;
      machines = lib.mapAttrs (name: machine: normalise (checkMachine name machine)) config.machines;
    };
in
{
  inherit metadataVersion optionsModule mkMetadata;

  # Hooks to use as `hooks.<name> = ...`.
  hooks = {
    # agenix-rekey (https://github.com/oddlama/agenix-rekey): rekey the
    # secrets after the target's host key is pulled (and recorded where the
    # configuration's `age.rekey.hostPubkey` reads it), before the build.
    # `command` runs the agenix-rekey CLI from your flake, e.g. a package
    # `agenix-rekey = config.agenix-rekey.package` with its flake-parts module.
    agenixRekey =
      {
        command ? "agenix-rekey",
      }:
      {
        inherit command;
        args = [
          "rekey"
          # Rekeyed secrets in local storage mode must be in git for the build.
          "--add-to-git"
        ];
        after = [ "host-key" ];
        before = [ "build" ];
      };
  };

  # Pinned mode: the device for disko disk `name`, as the controller recorded
  # it in `file` (the machine's `disks.file`). Until it has, the disk's
  # `/dev/disk/by-pixie/<name>` path, so the configuration evaluates and
  # builds before the first install.
  #
  #   disko.devices.disk.main.device = nix-pixie.lib.pinnedDisk ./disks.json "main";
  pinnedDisk =
    file: name:
    let
      pinned = if builtins.pathExists file then lib.importJSON file else { };
    in
    pinned.${name} or "/dev/disk/by-pixie/${name}";

  mkPixie =
    {
      nixosConfigurations,
      config ? { },
    }:
    mkMetadata {
      inherit nixosConfigurations;
      inherit
        (lib.evalModules {
          modules = [
            optionsModule
            config
          ];
        })
        config
        ;
    };
}
