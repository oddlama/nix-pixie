# nix-pixie, built with nixpkgs' Rust: the flake's package, and the agent of
# installer images (also cross-compiled, from the image's own nixpkgs, for a
# device of another platform). It takes nothing but nixpkgs, so a test VM
# evaluates it to the same derivation as the host that prebuilt it.
#
# The flake's checks build the crates with nci instead, which caches the
# dependencies, and run the tests there.
{
  lib,
  rustPlatform,
  iproute2,
  openssh,
  pkg-config,
  zstd,
}:
rustPlatform.buildRustPackage {
  pname = "nix-pixie";
  inherit ((lib.importTOML ../Cargo.toml).workspace.package) version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };
  cargoLock = {
    lockFile = ../Cargo.lock;
    # The iroh fork that keeps link-local scope ids (see docs/design.md).
    outputHashes."iroh-1.4.1" = "sha256-WbQy4tj/LJb/oJ6Sc3UoS1h5jkReV1dHwjLKp5ymBnE=";
  };
  # zstd-sys links nixpkgs' libzstd rather than building its own.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ zstd ];
  cargoBuildFlags = [
    "--package"
    "nix-pixie"
  ];
  doCheck = false;
  env = {
    # Discovery reads the neighbour table with `ip`, and the host key check
    # after an install uses ssh-keyscan; pinned.
    NIX_PIXIE_IP = lib.getExe' iproute2 "ip";
    NIX_PIXIE_SSH_KEYSCAN = lib.getExe' openssh "ssh-keyscan";
  };
  meta = {
    description = "Provision NixOS machines from an empty disk to a running system";
    license = lib.licenses.mit;
    mainProgram = "nix-pixie";
    platforms = lib.platforms.linux;
  };
}
