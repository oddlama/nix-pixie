# The nix-pixie installer ISO: the minimal NixOS installer plus the agent.
{
  config,
  lib,
  modulesPath,
  pkgs,
  ...
}:
{
  imports = [
    "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix"
    ./installer.nix
  ];

  image.baseName = lib.mkForce "nix-pixie-installer-${config.system.nixos.label}-${pkgs.stdenv.hostPlatform.system}";
  isoImage.appendToMenuLabel = " (nix-pixie installer)";
}
