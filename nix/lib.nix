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
        };
    in
    {
      version = metadataVersion;
      machines = lib.mapAttrs (name: machine: normalise (checkMachine name machine)) config.machines;
    };
in
{
  inherit metadataVersion optionsModule mkMetadata;

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
