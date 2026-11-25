let
  root = builtins.getFlake (toString ../..);
  # Use upstream packages: the signing shells' patched Git needs a source build.
  pkgs = import root.inputs.nixpkgs {
    system = builtins.currentSystem;
    config = {};
    overlays = [];
  };
in with pkgs; [
  bash
  coreutils
  diffutils
  just
  jq
  git
  yq-go
  (python3.withPackages (ps: [ ps.cryptography ]))
]
