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
}
