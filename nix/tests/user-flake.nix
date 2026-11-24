# The operator's flake for VM tests that install: nixosConfigurations built
# from a module in `targets/` (with disko and nix-pixie's system module) plus
# the `pixie` output, as a directory the test puts into the controller VM.
#
# The controller evaluates it inside its VM, like on a real controller, but
# must not build anything there (max-jobs = 0). So the same configurations are
# evaluated here as well and their outputs go into the VM's store. Both
# evaluations must yield the same derivations, which is why the target module
# may not depend on where it lives. If they diverge, `nix build` in the VM
# fails at once and lists what it would have to build.
{
  pkgs,
  # Flake inputs, for their original store paths and NAR hashes.
  nixpkgs,
  disko,
}:
{
  # A file in `targets/`, imported by every configuration. It sets everything
  # but the disks' devices.
  target,
  # name -> { disks.<disko disk> = {
  #             device = <Nix source for the device, with `pixieLib` in scope>;
  #             built = <the device it evaluates to when the test prebuilds>;
  #           };
  #           # For the prebuild only: what hooks will have changed in the
  #           # repository by the time the controller builds.
  #           builtModules = [ ];
  #         }
  configurations,
  # A file in `targets/` with `{ pkgs }: { <name> = <package>; }`: the
  # flake's packages, e.g. hooks. Prebuilt as well.
  packages ? null,
  # Nix source for mkPixie's `config.machines`.
  machines,
}:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  # The original store path, not `pkgs.path` coerced to a string: that is a
  # copy under another name, which `nix flake lock` in the VM copies yet
  # again (47 s and 340 MB of the VM's RAM). The original is just looked up.
  nixpkgsSource = nixpkgs.outPath;

  # What `nix flake lock` writes for the inputs below, so the VM needn't.
  pathInput = input: {
    locked = {
      lastModified = 0;
      inherit (input) narHash;
      path = input.outPath;
      type = "path";
    };
    original = {
      path = input.outPath;
      type = "path";
    };
  };
  flakeLock = pkgs.writeText "flake.lock" (
    builtins.toJSON {
      nodes = {
        nixpkgs = pathInput nixpkgs;
        disko = pathInput disko // {
          flake = false;
        };
        root.inputs = {
          nixpkgs = "nixpkgs";
          disko = "disko";
        };
      };
      root = "root";
      version = 7;
    }
  );

  evalTarget =
    devices: extraModules:
    (import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = null;
      modules = [
        "${disko}/module.nix"
        ../nixos/system.nix
        (./targets + "/${target}")
        {
          nixpkgs.hostPlatform = system;
          disko.devices.disk = lib.mapAttrs (_: device: { inherit device; }) devices;
        }
      ]
      ++ extraModules;
    }).config;

  prebuilt = lib.mapAttrs (
    _: c: evalTarget (lib.mapAttrs (_: d: d.built) c.disks) (c.builtModules or [ ])
  ) configurations;
  prebuiltPackages = lib.optionalAttrs (packages != null) (
    import (./targets + "/${packages}") { inherit pkgs; }
  );

  flakeNix = pkgs.writeText "flake.nix" ''
    {
      inputs = {
        nixpkgs.url = "path:${nixpkgsSource}";
        disko = {
          url = "path:${disko}";
          flake = false;
        };
      };
      outputs =
        { self, nixpkgs, disko }:
        let
          pixieLib = import ./nix-pixie/lib.nix { inherit (nixpkgs) lib; };
          target =
            devices:
            import "''${nixpkgs}/nixos/lib/eval-config.nix" {
              system = null;
              modules = [
                "''${disko}/module.nix"
                ./nix-pixie/nixos/system.nix
                ./targets/${target}
                {
                  nixpkgs.hostPlatform = "${system}";
                  disko.devices.disk = nixpkgs.lib.mapAttrs (_: device: { inherit device; }) devices;
                }
              ];
            };
        in
        {
          nixosConfigurations = {
            ${lib.concatStrings (
              lib.mapAttrsToList (name: c: ''
                ${name} = target {
                  ${lib.concatStrings (lib.mapAttrsToList (disk: d: "${disk} = ${d.device};\n") c.disks)}
                };
              '') configurations
            )}
          };
          ${lib.optionalString (packages != null) ''
            packages.${system} = import ./targets/${packages} {
              pkgs = nixpkgs.legacyPackages.${system};
            };
          ''}
          pixie = pixieLib.mkPixie {
            inherit (self) nixosConfigurations;
            config.machines = ${machines};
          };
        };
    }
  '';

  # nix-pixie's lib is vendored in: the test VM can't fetch it as an input.
  flakeDir = pkgs.runCommand "nix-pixie-test-flake" { } ''
    mkdir -p $out/nix-pixie/nixos
    cp ${flakeNix} $out/flake.nix
    cp ${flakeLock} $out/flake.lock
    # Readable, and a syntax error fails here rather than inside the VM.
    ${lib.getExe pkgs.nixfmt} $out/flake.nix
    cp -r ${./targets} $out/targets
    cp ${../lib.nix} $out/nix-pixie/lib.nix
    cp ${../options.nix} $out/nix-pixie/options.nix
    cp ${../nixos/system.nix} $out/nix-pixie/nixos/system.nix
    cp ${../nixos/disks.nix} $out/nix-pixie/nixos/disks.nix
  '';
in
{
  inherit prebuilt flakeDir;

  # For the controller node.
  controllerModule = {
    # Evaluating a NixOS configuration takes about 1 GiB.
    virtualisation.memorySize = 2048;
    environment.systemPackages = [ pkgs.git ];
    users.users.alice.isNormalUser = true;
    nix.settings = {
      # No experimental features (and so no flake-registry setting either):
      # the controller enables what it uses. The flake is locked to `path:`
      # inputs, so the registry is never asked.
      substituters = lib.mkForce [ ];
      # Fail at once instead of building a whole system in the VM.
      max-jobs = 0;
    };
    system.extraDependencies = [
      nixpkgsSource
      disko
    ]
    ++ lib.concatMap (c: [
      c.system.build.diskoScript
      c.system.build.toplevel
    ]) (lib.attrValues prebuilt)
    ++ lib.attrValues prebuiltPackages;
  };

  # Python: puts the flake into ~alice/repo as a git checkout.
  setupScript = ''
    controller.succeed(
        "su - alice -c 'cp -r --no-preserve=mode ${flakeDir} repo && cd repo"
        + " && git init -q && git add -A'"
    )
  '';
}
