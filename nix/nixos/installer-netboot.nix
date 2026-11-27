# The installer as netboot media: a kernel and an initrd that holds the whole
# system as a squashfs, mounted in stage 1, with a real stage 2. For devices
# booted over USB or the network rather than from a medium.
#
# Build it as a NixOS configuration of its own, for the device's platform and
# with its kernel and drivers, and use `system.build.nixPixieNetboot`: the
# kernel, the initrd, and `boot.kernelParams` as a command line, of which the
# installer needs nothing. Boot chains that don't start a kernel with a long
# command line keep theirs: stage 1 finds the system without `init=`, and its
# root is a tmpfs whatever `root=` says.
#
#   nixpkgs.lib.nixosSystem {
#     modules = [
#       nix-pixie.nixosModules.installer-netboot
#       {
#         nixpkgs.hostPlatform = "aarch64-linux";
#         boot.kernelPackages = ...;   # the device's kernel
#         nix-pixie.netboot.kernelModules = [ ... ];
#         nix-pixie.usbGadget.enable = true;
#       }
#     ];
#   }
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:
let
  cfg = config.nix-pixie.netboot;
  # Where stage 1 learns the system to boot when the command line doesn't
  # say. A file in the netboot initrd, which the system itself can't name.
  init = pkgs.writeText "nix-pixie-init" "${config.system.build.toplevel}/init\n";
  kernel = config.boot.kernelPackages.kernel;
  kernelModules = lib.getOutput "modules" kernel;
  # See `boot.initrd.systemd.root` below.
  rootGenerator = pkgs.writeScript "nix-pixie-root-generator" ''
    #!${pkgs.runtimeShell}
    early=$2
    ${pkgs.coreutils}/bin/mkdir -p "$early/initrd-root-fs.target.requires" \
      "$early/initrd-root-device.target.d"
    printf '%s\n' '[Unit]' 'Before=initrd-root-fs.target' \
      '[Mount]' 'What=tmpfs' 'Where=/sysroot' 'Type=tmpfs' 'Options=mode=0755' \
      > "$early/sysroot.mount"
    ${pkgs.coreutils}/bin/ln -sf ../sysroot.mount "$early/initrd-root-fs.target.requires/sysroot.mount"
    printf '# The root is a tmpfs, whatever root= names.\n' \
      > "$early/initrd-root-device.target.d/50-root-device.conf"
  '';
  # In the squashfs for nothing (see `squashfsStore`).
  unneeded = [
    config.system.build.initialRamdisk
  ]
  ++ lib.optional (kernelModules != kernel) kernel;
  # Only the modules asked for, and what they need. The tree stage 1 picks
  # its own modules from in turn, so it keeps the kernel's lists of
  # modules, which the closure leaves out.
  chosenModules =
    let
      wanted =
        cfg.kernelModules
        ++ installerModules
        ++ config.boot.initrd.availableKernelModules
        ++ config.boot.initrd.kernelModules
        ++ config.boot.kernelModules;
      closure =
        (pkgs.makeModulesClosure {
          kernel = pkgs.aggregateModules ([ kernelModules ] ++ config.boot.extraModulePackages);
          firmware = config.hardware.firmware;
          rootModules = lib.filter (name: !lib.hasInfix "/" name) wanted;
          # Built into the kernel, or not in this one at all.
          allowMissing = true;
        }).overrideAttrs
          (old: {
            # Directories: every module in them, by name, for nixpkgs' script.
            moduleDirs = lib.filter (lib.hasInfix "/") wanted;
            builder = pkgs.writeText "nix-pixie-modules-closure.sh" ''
              for dir in $moduleDirs; do
                for module in $(find -L "$kernel"/lib/modules/*/"$dir" -name '*.ko*' 2>/dev/null); do
                  module=''${module##*/}
                  rootModules="$rootModules ''${module%%.ko*}"
                done
              done
              source ${old.builder}
            '';
          });
    in
    pkgs.runCommand "${kernel.name}-chosen-modules" { } ''
      cp -r --no-preserve=mode ${closure} $out
      cd ${kernelModules}/lib/modules
      for version in *; do
        for list in modules.order modules.builtin modules.builtin.modinfo; do
          cp $version/$list $out/lib/modules/$version/
        done
      done
    '';
  # What the installer itself loads, some of it on demand (by the name of a
  # cipher, a filesystem, a netfilter match, a socket's family): every
  # device-mapper target and RAID level, the crypto dm-crypt and cryptsetup
  # ask for, the firewall's netfilter, packet sockets for DHCP, systemd's
  # default queueing, the common filesystems, and the netboot's own.
  installerModules = [
    "af_packet"
    "sch_fq_codel"
    "nls_utf8"
    "kernel/drivers/md"
    "kernel/crypto"
    "kernel/arch/${pkgs.stdenv.hostPlatform.linuxArch}/crypto"
    "kernel/net/netfilter"
    "kernel/net/ipv4/netfilter"
    "kernel/net/ipv6/netfilter"
    "ext4"
    "vfat"
    "nls_cp437"
    "nls_iso8859_1"
    "btrfs"
    "xfs"
    "loop"
    "squashfs"
    "overlay"
    "efivarfs"
  ];
in
{
  imports = [
    "${modulesPath}/installer/netboot/netboot.nix"
    "${modulesPath}/profiles/minimal.nix"
    # No Perl: etc as an overlay, users from userborn, and none of the
    # nixos-* tools (the agent brings nixos-install and nixos-enter).
    "${modulesPath}/profiles/perlless.nix"
    ./installer.nix
  ];

  options.nix-pixie.netboot.kernelModules = lib.mkOption {
    type = lib.types.nullOr (lib.types.listOf lib.types.str);
    default = null;
    example = [
      "r8168"
      "nvme"
      "tegra-xudc"
      "kernel/drivers/net/usb"
    ];
    description = ''
      Only these kernel modules go into the installer, with what they
      depend on and need as firmware, and those the installer loads itself
      (device mapper, RAID, crypto, netfilter, the common filesystems) and
      stage 1's. A name with a `/` is a directory of the kernel's module
      tree: all modules in it. The kernel's modules are most of an
      installer's size, so a boot image with a limit may need this. `null`:
      all of the kernel's.

      Firmware is not filtered: `hardware.firmware` goes into the installer
      whole.
    '';
  };

  config = {
    # Small enough for boot images with a limit (some take about 250 MB of
    # initrd): nixpkgs' own source stays out (it would only be the flake
    # registry's and NIX_PATH's), and xz packs the squashfs tighter.
    nixpkgs.flake = {
      setFlakeRegistry = false;
      setNixPath = false;
    };
    # nixos-rebuild alone is 144 MB of Python; the shell keeps the two the
    # agent runs.
    system.disableInstallerTools = lib.mkDefault true;
    environment.systemPackages = [
      pkgs.nixos-install
      pkgs.nixos-enter
    ];
    netboot.squashfsCompression = lib.mkDefault "xz -Xdict-size 100%";

    system.modulesTree = lib.mkIf (cfg.kernelModules != null) (lib.mkForce [ chosenModules ]);

    # A shell on tty2 and further, for looking around; tty1 shows the agent.
    services.getty.autologinUser = lib.mkDefault "root";

    # No `root=` needed, and none obeyed: a board's own command line may
    # name its eMMC, and with that systemd-fstab-generator would mount it as
    # the root, and wait for it. A generator of ours writes into the "early"
    # directory, which outranks the fstab generator's: a tmpfs as the root,
    # and an empty drop-in of the name it waits for the root device with.
    # (With a `root=`, the fstab generator also complains about a duplicate
    # /sysroot, harmlessly.)
    boot.initrd.systemd.root = null;
    boot.initrd.systemd.contents."/etc/systemd/system-generators/nix-pixie-root".source = rootGenerator;
    boot.initrd.systemd.storePaths = [
      pkgs.runtimeShell
      "${pkgs.coreutils}/bin/mkdir"
      "${pkgs.coreutils}/bin/ln"
    ];

    # Without `init=` on the command line, give stage 1 the system's: a
    # command line with it appended, bound over /proc/cmdline before NixOS's
    # stage 1 reads it.
    boot.initrd.systemd.services.nix-pixie-init = {
      description = "nix-pixie: the installer's system, without init= on the command line";
      requiredBy = [ "initrd-find-nixos-closure.service" ];
      before = [ "initrd-find-nixos-closure.service" ];
      unitConfig = {
        DefaultDependencies = false;
        ConditionPathExists = "/nix-pixie-init";
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        read -r cmdline < /proc/cmdline
        case " $cmdline " in
          *" init="*) exit 0 ;;
        esac
        read -r init < /nix-pixie-init || [ -n "$init" ]
        printf '%s init=%s\n' "$cmdline" "$init" > /run/nix-pixie-cmdline
        ${config.boot.initrd.systemd.package.util-linux}/bin/mount --bind /run/nix-pixie-cmdline /proc/cmdline
      '';
    };

    # The system's closure, but for its kernel and its initrd, which are
    # beside the squashfs already and which stage 2 doesn't need: some 45 MB
    # that wouldn't compress. Nix's database still lists them, which nothing
    # here minds: the installer never copies its own system. A kernel whose
    # modules aren't an output of their own stays: they are needed.
    system.build.squashfsStore = lib.mkForce (
      pkgs.stdenvNoCC.mkDerivation {
        name = "nix-pixie-squashfs.img";
        __structuredAttrs = true;
        unsafeDiscardReferences.out = true;
        nativeBuildInputs = [ pkgs.buildPackages.squashfs-tools ];
        buildCommand = ''
          closureInfo=${pkgs.closureInfo { rootPaths = config.netboot.storeContents; }}
          cp $closureInfo/registration nix-path-registration
          grep -v -x -F ${lib.escapeShellArgs (map (path: "-e${path}") unneeded)} \
            $closureInfo/store-paths > store-paths
          SOURCE_DATE_EPOCH=0 mksquashfs nix-path-registration $(cat store-paths) $out \
            -no-hardlinks -keep-as-directory -all-root -b 1048576 \
            -comp ${config.netboot.squashfsCompression} -processors $NIX_BUILD_CORES -root-mode 0755
        '';
      }
    );

    system.build.netbootRamdisk = lib.mkForce (
      pkgs.makeInitrdNG {
        inherit (config.boot.initrd) compressor compressorArgs;
        prepend = [ "${config.system.build.initialRamdisk}/initrd" ];
        contents = [
          {
            source = config.system.build.squashfsStore;
            target = "/nix-store.squashfs";
          }
          {
            source = init;
            target = "/nix-pixie-init";
          }
        ];
      }
    );

    system.build.nixPixieNetboot = pkgs.runCommand "nix-pixie-netboot" { } ''
      mkdir -p $out
      ln -s ${config.system.build.kernel}/${config.system.boot.loader.kernelFile} $out/kernel
      ln -s ${config.system.build.netbootRamdisk}/initrd $out/initrd
      echo -n ${lib.escapeShellArg (toString config.boot.kernelParams)} > $out/cmdline
    '';
  };
}
