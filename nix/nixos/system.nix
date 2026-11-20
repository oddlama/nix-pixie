# For the configurations nix-pixie installs (`nixosModules.default`): what the
# installed system needs to keep working the way it was installed.
{
  imports = [ ./disks.nix ];
}
