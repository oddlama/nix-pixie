# What a live system needs so disko can build any storage stack on it, beyond
# the agent's own tools: mdadm's udev rules, without which /dev/md/<name>
# never appears and disko times out waiting for it. Part of the installer
# module; nixpkgs' installer profiles set the same.
{ lib, pkgs, ... }:
{
  boot.swraid = {
    enable = lib.mkDefault true;
    # mdadm --monitor needs somewhere to report to; a live system has none.
    mdadmConf = lib.mkDefault "PROGRAM ${pkgs.coreutils}/bin/true";
  };
}
