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
    installer = throw "nixosConfigurations.installer was evaluated";
  };

  pixieConfig = {
    # For every machine, by its name; the machines' own settings win.
    defaults =
      { name, ... }:
      {
        hostKey.publicKeyPath = "machines/${name}/host.pub";
        facter.reportPath = "machines/${name}/facter.json";
      };
    hooks = {
      rekey = pixieLib.hooks.agenixRekey { };
      notify = {
        command = "notify";
        after = [ "boot" ];
      };
    };
    machines = {
      web1 = {
        match.mac = [ "AA:BB:CC:DD:EE:FF" ];
        hostKey = {
          mode = "pull";
          publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
        };
        facter.reportPath = "machines/web1/facter.json";
        # Overrides one option of a hook for every machine.
        hooks.rekey.args = [ "rekey" ];
      };
      rack-a.configuration = "generic";
      db1 = {
        configuration = "web1";
        disks.file = "machines/db1/disks.json";
        hooks.notify.enable = false;
        # Back to the option's own default.
        facter.reportPath = null;
      };
      # A configuration that isn't for installing.
      installer.enable = false;
      # `generic` isn't mentioned: it is a machine of its own all the same.
    };
  };

  rekey = {
    command = "agenix-rekey";
    args = [
      "rekey"
      "--add-to-git"
    ];
    after = [ "host-key" ];
    before = [ "build" ];
  };
  notify = {
    command = "notify";
    args = [ ];
    after = [ "boot" ];
    before = [ ];
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
          serial = [ ];
        };
        disks.file = null;
        hostKey = {
          mode = "pull";
          publicKeyPath = "machines/web1/ssh_host_ed25519_key.pub";
        };
        facter.reportPath = "machines/web1/facter.json";
        hooks = {
          rekey = rekey // {
            args = [ "rekey" ];
          };
          inherit notify;
        };
      };
      rack-a = {
        configuration = "generic";
        match = {
          mac = [ ];
          dmiSerial = [ ];
          serial = [ ];
        };
        disks.file = null;
        hostKey = {
          mode = "none";
          publicKeyPath = "machines/rack-a/host.pub";
        };
        facter.reportPath = "machines/rack-a/facter.json";
        hooks = { inherit rekey notify; };
      };
      generic = {
        configuration = "generic";
        match = {
          mac = [ ];
          dmiSerial = [ ];
          serial = [ ];
        };
        disks.file = null;
        hostKey = {
          mode = "none";
          publicKeyPath = "machines/generic/host.pub";
        };
        facter.reportPath = "machines/generic/facter.json";
        hooks = { inherit rekey notify; };
      };
      db1 = {
        configuration = "web1";
        match = {
          mac = [ ];
          dmiSerial = [ ];
          serial = [ ];
        };
        disks.file = "machines/db1/disks.json";
        hostKey = {
          mode = "none";
          publicKeyPath = "machines/db1/host.pub";
        };
        facter.reportPath = null;
        hooks = { inherit rekey; };
      };
    };
  };

  # `defaults` as plain modules: conditions, imports, and a machine that
  # overrides one of them.
  plainDefaults =
    (pixieLib.mkPixie {
      nixosConfigurations = {
        a = throw "a was evaluated";
        b = throw "b was evaluated";
      };
      config = {
        defaults = {
          imports = [ { match.dmiSerial = [ "imported" ]; } ];
          config = lib.mkIf true { hostKey.mode = "pull"; };
        };
        machines.b.hostKey.mode = "push";
      };
    }).machines;

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
    hook-without-command = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.hooks.broken.after = [ "build" ];
      config.machines.web1 = { };
    };
    absolute-disks-file = pixieLib.mkPixie {
      inherit nixosConfigurations;
      config.machines.web1.disks.file = "/etc/disks.json";
    };
  };

  # agenixRekey.hostPubkey, against an option like agenix-rekey's
  # `age.rekey.hostPubkey` (modules/agenix-rekey.nix): a path is read, the
  # default is a dummy key.
  dummyPubkey = "age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqs3290gq";
  hostPubkey =
    definition:
    (lib.evalModules {
      modules = [
        {
          options.hostPubkey = lib.mkOption {
            type = with lib.types; coercedTo path (x: if builtins.isPath x then builtins.readFile x else x) str;
            default = dummyPubkey;
          };
          config.hostPubkey = definition;
        }
      ];
    }).config.hostPubkey;
  pubkeys = {
    # A pulled key, as the controller records it.
    recorded = hostPubkey (pixieLib.agenixRekey.hostPubkey ./host.pub);
    notYet = hostPubkey (pixieLib.agenixRekey.hostPubkey ./does-not-exist.pub);
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
assert lib.assertMsg (
  plainDefaults.a.hostKey.mode == "pull"
  && plainDefaults.b.hostKey.mode == "push"
  && plainDefaults.b.match.dmiSerial == [ "imported" ]
) "plain defaults gave ${json plainDefaults}";
assert lib.assertMsg (
  pubkeys == {
    recorded = builtins.readFile ./host.pub;
    notYet = dummyPubkey;
  }
) "agenixRekey.hostPubkey gave ${json pubkeys}";
pkgs.writeText "eval-metadata.json" (json viaLib)
