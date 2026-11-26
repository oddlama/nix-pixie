# lanzaboote from its source rather than its flake, for the test flake: its
# flake would bring its own nixpkgs and four more inputs into the VM. Its
# `default.nix` fetches crane and rust-overlay as its flake.lock pins them,
# so the VM only needs those sources in its store. The same file serves the
# prebuild and the VM, so both evaluate the same derivations.
lanzaboote:
{ pkgs, ... }:
{
  imports = [ "${lanzaboote}/nix/modules/lanzaboote.nix" ];
  boot.lanzaboote.package =
    (import lanzaboote {
      inherit pkgs;
      inherit (pkgs.stdenv.hostPlatform) system;
    }).packages.lzbt;
}
