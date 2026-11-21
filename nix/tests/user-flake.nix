# The operator's flake for VM tests that install: nixosConfigurations built
# from `targetModule` (with disko and nix-pixie's system module) plus the
# `pixie` output, as a directory the test puts into the controller VM.
#
# The controller evaluates it inside its VM, like on a real controller, but
# must not build anything there (max-jobs = 0). So the same configurations are
# evaluated here as well and their outputs go into the VM's store. Both
# evaluations must yield the same derivations, which is why the target module
# may not depend on where it lives. If they diverge, `nix build` in the VM
# fails at once and lists what it would have to build.
{ pkgs, disko }:
{
  # Imported by every configuration; sets everything but disk "main"'s device.
  targetModule,
  # name -> { device = <Nix source for the device, with `pixieLib` in scope>;
  #           built = <the device it evaluates to when the test prebuilds>; }
  configurations,
  # Nix source for mkPixie's `config.machines`.
  machines,
}:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  nixpkgsSource = "${pkgs.path}";

  evalTarget =
    device:
    (import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = null;
      modules = [
        "${disko}/module.nix"
        ../nixos/system.nix
        targetModule
        {
          nixpkgs.hostPlatform = system;
          disko.devices.disk.main.device = device;
        }
      ];
    }).config;

  prebuilt = lib.mapAttrs (_: c: evalTarget c.built) configurations;

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
            device:
            import "''${nixpkgs}/nixos/lib/eval-config.nix" {
              system = null;
              modules = [
                "''${disko}/module.nix"
                ./nix-pixie/nixos/system.nix
                ./target.nix
                {
                  nixpkgs.hostPlatform = "${system}";
                  disko.devices.disk.main.device = device;
                }
              ];
            };
        in
        {
          nixosConfigurations = {
            ${lib.concatStrings (
              lib.mapAttrsToList (name: c: ''
                ${name} = target (${c.device});
              '') configurations
            )}
          };
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
    cp ${targetModule} $out/target.nix
    cp ${../lib.nix} $out/nix-pixie/lib.nix
    cp ${../options.nix} $out/nix-pixie/options.nix
    cp ${../nixos/system.nix} $out/nix-pixie/nixos/system.nix
    cp ${../nixos/disks.nix} $out/nix-pixie/nixos/disks.nix
  '';
in
{
  inherit prebuilt;

  # For the controller node.
  controllerModule = {
    virtualisation.memorySize = 3072;
    environment.systemPackages = [ pkgs.git ];
    users.users.alice.isNormalUser = true;
    nix.settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      flake-registry = "";
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
    ]) (lib.attrValues prebuilt);
  };

  # Python: puts the flake into ~alice/repo as a git checkout, locked.
  setupScript = ''
    controller.succeed(
        "su - alice -c 'cp -r --no-preserve=mode ${flakeDir} repo && cd repo"
        + " && git init -q && git add -A && nix flake lock && git add flake.lock'"
    )
  '';
}
