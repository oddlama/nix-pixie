# The hooks of `checks.vm-install-hooks`, as the test flake's packages. Each
# copies its context to /tmp/context-<name>.json on the controller, for the
# test to inspect. writeShellScriptBin rather than writeShellApplication:
# the latter's shellcheck makes the controller VM evaluate the Haskell
# package set, which takes minutes there.
{ pkgs }:
let
  hook =
    name: text:
    pkgs.writeShellScriptBin name ''
      set -euo pipefail
      export PATH=${
        pkgs.lib.makeBinPath [
          pkgs.git
          pkgs.coreutils
        ]
      }:$PATH
      cp "$PIXIE_CONTEXT" /tmp/context-${name}.json
      ${text}
    '';
in
{
  # Before the build: changes the repository, and stages it for the build.
  stamp = hook "stamp" ''
    mkdir -p machines/web1
    echo "stamped before the build" > machines/web1/stamp
    git add machines/web1/stamp
  '';
  # Before the disks are formatted: a file for the installed system.
  secret = hook "secret" ''
    mkdir -p "$PIXIE_OUT/files/var/lib/hook"
    printf "from the hook" > "$PIXIE_OUT/files/var/lib/hook/secret"
    chmod 0640 "$PIXIE_OUT/files/var/lib/hook/secret"
  '';
  # After the installed system answered.
  booted = hook "booted" "";
  # Programs on the target, on the installer: output and exit codes come
  # back, and nothing but the store and a clean environment is there.
  on-target = hook "on-target" ''
    name=$("$PIXIE" target exec -- ${pkgs.coreutils}/bin/uname -n)
    [ "$name" = target ] || { echo "on-target: uname said $name" >&2; exit 1; }
    out=$("$PIXIE" target exec -- ${pkgs.bash}/bin/bash -c 'echo out; echo err >&2' 2>/dev/null)
    [ "$out" = out ] || { echo "on-target: stdout was $out" >&2; exit 1; }
    env=$("$PIXIE" target exec -- ${pkgs.coreutils}/bin/env | ${pkgs.coreutils}/bin/sort | ${pkgs.coreutils}/bin/tr '\n' ' ')
    [ "$env" = "HOME=/root LANG=C.UTF-8 PATH=/var/empty " ] || { echo "on-target: env was $env" >&2; exit 1; }
    if "$PIXIE" target exec -- ${pkgs.coreutils}/bin/false; then exit 1; fi
    if "$PIXIE" target exec -- /bin/sh -c true; then exit 1; fi
    "$PIXIE" target exec -- ${pkgs.coreutils}/bin/touch /tmp/from-hook
    "$PIXIE" target exec -- ${pkgs.coreutils}/bin/test -e /tmp/from-hook
  '';
  # Programs in the installed system, once it is installed.
  in-system = hook "in-system" ''
    "$PIXIE" target exec --chroot -- ${pkgs.coreutils}/bin/mkdir -p /var/lib/pixie
    "$PIXIE" target exec --chroot -- ${pkgs.coreutils}/bin/touch /var/lib/pixie/from-hook
  '';
  # Runs on the target while the test cuts the network for longer than a
  # silent connection lasts. Its reply arrives once the session resumed.
  outage = hook "outage" ''
    "$PIXIE" target exec -- ${pkgs.coreutils}/bin/sleep 50.5
  '';
  # Fails the first time, like a hook whose password was mistyped.
  flaky = hook "flaky" ''
    if [ ! -e /tmp/flaky-tried ]; then
      touch /tmp/flaky-tried
      echo "flaky: failing on purpose" >&2
      exit 1
    fi
  '';

  # Push mode: the host key. A throwaway key, committed for the tests only.
  push-key = hook "push-key" ''
    cp ${./pushed-host-key} "$PIXIE_OUT/host-key"
  '';
  # The LUKS passphrase disko reads at /tmp/secret.key (targets/luks.nix).
  luks-key = hook "luks-key" ''
    mkdir -p "$PIXIE_OUT/disk-keys/tmp"
    printf secretsecret > "$PIXIE_OUT/disk-keys/tmp/secret.key"
  '';
}
