# Both entry points (plain `lib.mkPixie` and the flake-parts module) must
# produce identical metadata from identical configuration. Also covers the
# other evaluation-only helpers of the lib.
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
        hostKey = {
          mode = "pull";
          publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
        };
        facter.reportPath = "machines/web1/facter.json";
      };
      rack-a.configuration = "generic";
      db1 = {
        configuration = "web1";
        disks.file = "machines/db1/disks.json";
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
        disks.file = null;
        hostKey = {
          mode = "pull";
          publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
        };
        facter.reportPath = "machines/web1/facter.json";
      };
      rack-a = {
        configuration = "generic";
        match = {
          mac = [ ];
          dmiSerial = [ ];
        };
        disks.file = null;
        hostKey = {
          mode = "none";
          publicKeyPath = null;
        };
        facter.reportPath = null;
      };
      db1 = {
        configuration = "web1";
        match = {
          mac = [ ];
          dmiSerial = [ ];
        };
        disks.file = "machines/db1/disks.json";
        hostKey = {
          mode = "none";
          publicKeyPath = null;
        };
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
    escaping-public-key-path = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.hostKey.publicKeyPath = "../web1.pub";
    };
    absolute-disks-file = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.disks.file = "/etc/disks.json";
    };
  };

  # pinnedDisk: the recorded disk, else the role path; also before the file exists.
  disksFile = builtins.toFile "disks.json" (json {
    main = "/dev/disk/by-id/nvme-Example_SSD_1234";
  });
  pinned = {
    recorded = pixieLib.pinnedDisk disksFile "main";
    unrecorded = pixieLib.pinnedDisk disksFile "data";
    noFile = pixieLib.pinnedDisk ./does-not-exist.json "main";
  };
  expectedPinned = {
    recorded = "/dev/disk/by-id/nvme-Example_SSD_1234";
    unrecorded = "/dev/disk/by-pixie/data";
    noFile = "/dev/disk/by-pixie/main";
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
assert lib.assertMsg (pinned == expectedPinned) "pinnedDisk returned ${json pinned}";
pkgs.writeText "eval-metadata.json" (json viaLib)
