# The enclave kernel is pinned independently of the Nixpkgs snapshot so Linux
# stable fixes do not wait for the NixOS package-update cadence. Update all four
# fields together after reviewing the upstream 6.12 LTS changelog.
{
  branch = "6.12";
  version = "6.12.112";
  url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.112.tar.xz";
  hash = "sha256-Fk3J0fbJPGGhXh8HHEg3m0Z/KxfEacznIjRxloII7QM=";
}
