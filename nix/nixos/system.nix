# For the configurations nix-pixie installs (`nixosModules.default`): what the
# installed system needs to keep working the way it was installed, and to be
# checked after its first boot.
{
  imports = [
    ./disks.nix
    ./secure-boot.nix
  ];
}
