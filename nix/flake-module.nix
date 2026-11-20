# flakeModules.default: a thin wrapper around `lib.mkMetadata`.
{ pixieLib }:
{ config, lib, ... }:
{
  options.pixie = lib.mkOption {
    type = lib.types.submodule pixieLib.optionsModule;
    default = { };
    description = "nix-pixie machine definitions. See nix-pixie's docs/design.md.";
  };

  config.flake.pixie = pixieLib.mkMetadata {
    config = config.pixie;
    nixosConfigurations = config.flake.nixosConfigurations or { };
  };
}
