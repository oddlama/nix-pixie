# `checks [PATTERN]` in the devshell: build the flake's checks whose names
# match PATTERN (an extended regex, default: all), in parallel.
#
# `nix flake check` and `nix build` of several VM tests evaluate them all in
# one process: about 1.4 GiB per VM test, several GiB for all of them, on top
# of the VMs. This evaluates each check in its own process and then builds
# the derivations, which needs no evaluation, `JOBS` at a time (default 2;
# each VM test uses about 3 GiB while it runs).
{ writeShellApplication, jq }:
writeShellApplication {
  name = "checks";
  runtimeInputs = [ jq ];
  # Uses the caller's `nix`, like everything else in the devshell.
  text = ''
    pattern=''${1:-.}
    jobs=''${JOBS:-2}
    system=$(nix eval --impure --raw --expr builtins.currentSystem)

    mapfile -t names < <(
      nix eval --json ".#checks.$system" --apply builtins.attrNames \
        | jq -r '.[]' | grep -E -- "$pattern" || true
    )
    if [ ''${#names[@]} -eq 0 ]; then
      echo "no check matches $pattern" >&2
      exit 1
    fi

    drvs=()
    for name in "''${names[@]}"; do
      echo "evaluating $name" >&2
      drvs+=("$(nix eval --raw ".#checks.$system.$name.drvPath")^out")
    done

    echo "building ''${#names[@]} checks, $jobs at a time: ''${names[*]}" >&2
    nix build --no-link --print-build-logs --keep-going --max-jobs "$jobs" "''${drvs[@]}"
  '';
}
