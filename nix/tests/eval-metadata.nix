# Both entry points (plain `lib.mkPixie` and the flake-parts module) must
# produce identical metadata from identical configuration.
{
  pkgs,
  pixieLib,
  flake-parts,
  flakeModule,
}:
let
  inherit (pkgs) lib;

  # Stand-ins: the metadata only looks at attribute names, so these must never
  # be forced. `throw` makes the test fail if they are.
  nixosConfigurations = {
    web1 = throw "nixosConfigurations.web1 was evaluated";
    generic = throw "nixosConfigurations.generic was evaluated";
  };

  pixieConfig = {
    machines = {
      web1 = {
        match.mac = [ "AA:BB:CC:DD:EE:FF" ];
        hostKey.mode = "pull";
        facter.reportPath = "machines/web1/facter.json";
      };
      rack-a = {
        configuration = "generic";
        disks = {
          mode = "agnostic";
          roles = [
            "main"
            "data0"
          ];
        };
      };
    };
  };

  viaLib = pixieLib.mkPixie {
    inherit nixosConfigurations;
    config = pixieConfig;
  };

  viaFlakeParts =
    (flake-parts.lib.mkFlake { inputs.self = { }; } {
      imports = [ flakeModule ];
      systems = [ ];
      flake = { inherit nixosConfigurations; };
      pixie = pixieConfig;
    }).pixie;

  expected = {
    version = 1;
    machines = {
      web1 = {
        configuration = "web1";
        match = {
          mac = [ "aa:bb:cc:dd:ee:ff" ];
          dmiSerial = [ ];
        };
        disks = {
          mode = "pinned";
          roles = [ "main" ];
        };
        hostKey.mode = "pull";
        facter.reportPath = "machines/web1/facter.json";
      };
      rack-a = {
        configuration = "generic";
        match = {
          mac = [ ];
          dmiSerial = [ ];
        };
        disks = {
          mode = "agnostic";
          roles = [
            "main"
            "data0"
          ];
        };
        hostKey.mode = "none";
        facter.reportPath = null;
      };
    };
  };

  # Error cases: evaluation must fail.
  fails = expr: !(builtins.tryEval (builtins.deepSeq expr expr)).success;
  failures = {
    unknown-configuration = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.nope = { };
    };
    bad-mac = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.match.mac = [ "not-a-mac" ];
    };
    absolute-report-path = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.facter.reportPath = "/etc/facter.json";
    };
    escaping-report-path = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.facter.reportPath = "machines/../../facter.json";
    };
  };
  unexpectedSuccesses = lib.attrNames (lib.filterAttrs (_: expr: !(fails expr)) failures);

  json = builtins.toJSON;
in
assert lib.assertMsg (
  json viaLib == json expected
) "lib.mkPixie metadata differs from expected:\n${json viaLib}";
assert lib.assertMsg (
  json viaFlakeParts == json viaLib
) "flake-parts metadata differs from lib.mkPixie:\n${json viaFlakeParts}";
assert lib.assertMsg (
  unexpectedSuccesses == [ ]
) "these invalid configurations evaluated: ${lib.concatStringsSep ", " unexpectedSuccesses}";
pkgs.writeText "eval-metadata.json" (json viaLib)
