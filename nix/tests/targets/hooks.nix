# ext4, plus what the `stamp` hook writes into the repository before the
# build, as an agenix-rekey hook would write rekeyed secrets. Until the hook
# ran (and in the test's prebuild, which sets the text itself), "no stamp".
{ lib, ... }:
let
  stamp = ../machines/web1/stamp;
in
{
  imports = [ ./ext4.nix ];

  environment.etc."pixie-stamp".text = lib.mkDefault (
    if builtins.pathExists stamp then builtins.readFile stamp else "no stamp"
  );
}
