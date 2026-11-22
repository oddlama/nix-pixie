{
  description = "nix-pixie: provision NixOS machines from an empty disk to a running system";

  inputs = {
    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Only for the checks: nix-pixie works with whatever disko the user's
    # configurations import. Not a flake input, so it pulls in nothing else.
    disko = {
      url = "github:nix-community/disko";
      flake = false;
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    nci = {
      url = "github:90-008/nix-cargo-integration";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        parts.follows = "flake-parts";
        treefmt.follows = "treefmt-nix";
      };
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (
      { lib, ... }:
      let
        # Plain-Nix entry point. The flake-parts module below is a thin wrapper
        # around it, so both ways of using nix-pixie produce the same metadata.
        pixieLib = import ./nix/lib.nix { inherit lib; };

        withDefaultPackage =
          module:
          { lib, pkgs, ... }:
          {
            imports = [ module ];
            services.nix-pixie-agent.package =
              lib.mkDefault
                inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          };
      in
      {
        imports = [
          inputs.devshell.flakeModule
          inputs.nci.flakeModule
          inputs.treefmt-nix.flakeModule
        ];

        flake = {
          lib = pixieLib;
          # Only adds options and the `pixie` output. It deliberately imports
          # none of our development modules (nci, devshell, treefmt).
          flakeModules.default = import ./nix/flake-module.nix { inherit pixieLib; };

          # The package defaults to this flake's build for the host platform.
          nixosModules = {
            # For the configurations nix-pixie installs.
            default = ./nix/nixos/system.nix;
            # Just the agent service.
            agent = withDefaultPackage ./nix/nixos/agent.nix;
            # Agent plus installer networking: add to any live system.
            installer = withDefaultPackage ./nix/nixos/installer.nix;
          };
        };

        systems = [
          "x86_64-linux"
          "aarch64-linux"
        ];

        perSystem =
          {
            config,
            pkgs,
            system,
            ...
          }:
          let
            # The project is named apart from its crates: nci exposes both under
            # `nci.outputs`, and a project called `nix-pixie` would collide with
            # the `nix-pixie` crate.
            projectName = "workspace";
            crateOutputs = lib.filterAttrs (name: _: name != projectName) config.nci.outputs;
          in
          {
            nci = {
              toolchainConfig = ./rust-toolchain.toml;
              projects.${projectName} = {
                path = ./.;
                # Outputs are selected below instead of exported wholesale.
                export = false;
                numtideDevshell = "default";
                # Warnings fail every nix build of our crates, including the
                # clippy check. Registry dependencies cap their own lints, so
                # this only applies to code in this repository. Both derivations
                # get the same flags so cargo can reuse the prebuilt dependencies.
                drvConfig.env = {
                  RUSTFLAGS = "-D warnings";
                  # Discovery reads the neighbour table with `ip`, and the
                  # host key check after an install uses ssh-keyscan; pin them.
                  NIX_PIXIE_IP = lib.getExe' pkgs.iproute2 "ip";
                  NIX_PIXIE_SSH_KEYSCAN = lib.getExe' pkgs.openssh "ssh-keyscan";
                };
                depsDrvConfig.env.RUSTFLAGS = "-D warnings";
              };
            };

            packages = {
              default = lib.addMetaAttrs {
                mainProgram = "nix-pixie";
                license = lib.licenses.mit;
              } config.nci.outputs.nix-pixie.packages.release;

              # Boot this on the machine to install; it shows a pairing code.
              installer-iso =
                (inputs.nixpkgs.lib.nixosSystem {
                  modules = [
                    ./nix/nixos/installer-iso.nix
                    { services.nix-pixie-agent.package = config.packages.default; }
                    { nixpkgs.hostPlatform = system; }
                  ];
                }).config.system.build.isoImage;
            };

            # Every crate contributes its test suite and its clippy run.
            checks =
              lib.mapAttrs' (name: output: lib.nameValuePair "tests-${name}" output.check) crateOutputs
              // lib.mapAttrs' (name: output: lib.nameValuePair "clippy-${name}" output.clippy) crateOutputs
              // {
                eval-metadata = import ./nix/tests/eval-metadata.nix {
                  inherit pkgs pixieLib;
                  inherit (inputs) flake-parts;
                  flakeModule = inputs.self.flakeModules.default;
                };
                eval-disks = import ./nix/tests/eval-disks.nix {
                  inherit pkgs;
                  inherit (inputs) disko;
                  systemModule = ./nix/nixos/system.nix;
                };
                vm-pairing = import ./nix/tests/pairing.nix {
                  inherit pkgs;
                  package = config.packages.default;
                  agentModule = ./nix/nixos/agent.nix;
                };
                vm-inventory = import ./nix/tests/inventory.nix {
                  inherit pkgs pixieLib;
                  package = config.packages.default;
                  agentModule = ./nix/nixos/agent.nix;
                };
                vm-push = import ./nix/tests/push.nix {
                  inherit pkgs;
                  package = config.packages.default;
                  agentModule = ./nix/nixos/agent.nix;
                };
                vm-disko = import ./nix/tests/disko.nix {
                  inherit pkgs;
                  inherit (inputs) nixpkgs disko;
                  package = config.packages.default;
                  agentModule = ./nix/nixos/agent.nix;
                };
                vm-link-local = import ./nix/tests/link-local.nix {
                  inherit pkgs;
                  package = config.packages.default;
                  installerModule = ./nix/nixos/installer.nix;
                };
              }
              # Install, boot and check each disk layout.
              // lib.mapAttrs' (
                layout: _:
                lib.nameValuePair "vm-install-${layout}" (
                  import ./nix/tests/install.nix {
                    inherit pkgs layout;
                    inherit (inputs) nixpkgs disko;
                    package = config.packages.default;
                    agentModule = ./nix/nixos/agent.nix;
                  }
                )
              ) (import ./nix/tests/layouts.nix);

            devshells.default.packages = [
              config.treefmt.build.wrapper
              # `checks [PATTERN]`: build checks in parallel without
              # evaluating them all in one process.
              (pkgs.callPackage ./nix/checks.nix { })
            ];

            treefmt = {
              projectRootFile = "flake.nix";
              programs = {
                deadnix.enable = true;
                nixfmt.enable = true;
                statix.enable = true;
                taplo.enable = true;
                rustfmt = {
                  enable = true;
                  edition = "2024";
                };
              };
            };
          };
      }
    );
}
