# With lanzaboote's automatic enrollment (`autoEnrollKeys` with `autoReboot`),
# the first boot prepares the Secure Boot keys and reboots, for systemd-boot
# to enroll them. The SSH server waits for that preparation here: then it
# never answers on that first boot, so whoever checks the installed system by
# its host key (nix-pixie, after the install) sees the boot after the
# enrollment, with Secure Boot on. On later boots the preparation is skipped,
# and nothing waits.
{ config, lib, ... }:
let
  lanzaboote = config.boot.lanzaboote or { };
  rebootsToEnroll =
    (lanzaboote.enable or false)
    && (lanzaboote.autoEnrollKeys.enable or false)
    && (lanzaboote.autoEnrollKeys.autoReboot or false);
  ssh = config.services.openssh;
  after = [ "prepare-sb-auto-enroll.service" ];
in
{
  config = lib.mkIf (rebootsToEnroll && ssh.enable) {
    # Its reboot replaces what was still waiting.
    systemd.services.sshd = lib.mkIf (!ssh.startWhenNeeded) { inherit after; };
    systemd.sockets.sshd = lib.mkIf ssh.startWhenNeeded { inherit after; };
  };
}
