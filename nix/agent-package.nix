# The agent for live systems that aren't our installer, e.g. a cloud
# provider's stock NixOS ISO:
#
#   sudo nix --extra-experimental-features 'nix-command flakes' \
#     run github:oddlama/nix-pixie#agent
#
# What `nixosModules.agent` sets up as a service, done by hand: root, the
# tools the agent runs, and the firewall. The system's own tools come first
# on PATH (its `nix`, its `zpool` matching its kernel, its `modprobe`), also
# when started with a bare PATH (`systemd-run`); ours only fill the gaps. Also things the system has to provide itself: Nix, and
# for the respective layouts ZFS and mdadm's udev rules (the stock NixOS ISO
# has all of them).
{
  lib,
  writeShellApplication,
  nix-pixie,
  util-linux,
  kmod,
  openssh,
  nixos-install,
  nixos-enter,
  nixos-facter,
}:
writeShellApplication {
  name = "nix-pixie-agent";
  text = ''
    if [ "$EUID" -ne 0 ]; then
      echo "nix-pixie-agent: run it as root (it formats disks), e.g. with sudo" >&2
      exit 1
    fi
    export PATH="$PATH:/run/wrappers/bin:/run/current-system/sw/bin:${
      lib.makeBinPath [
        util-linux
        kmod
        openssh
        nixos-install
        nixos-enter
        nixos-facter
      ]
    }"

    # The controller reaches the agent on UDP 47011 (the agent's endpoint;
    # with `--port`, open that one yourself) and 47012 (probes). NixOS's
    # firewall drops them unless opened; other firewalls (a cloud
    # provider's, say) are up to you. Not mDNS (5353): this may be a server
    # on the internet, and mdns-sd answers unicast queries from anywhere
    # with more than it got, a reflector. Probes find us on a LAN anyway.
    if command -v nixos-firewall-tool >/dev/null; then
      for port in 47011 47012; do
        nixos-firewall-tool open udp "$port"
      done
    else
      echo "nix-pixie-agent: make sure UDP 47011 and 47012 reach this machine" >&2
    fi

    exec ${lib.getExe nix-pixie} agent "$@"
  '';
}
