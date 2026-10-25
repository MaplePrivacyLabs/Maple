{ pkgs }:
let
  manifest = builtins.fromJSON (builtins.readFile ../assets/manifest.json);
  assets = map
    (asset: {
      name = asset.filename;
      path = pkgs.fetchurl {
        urls = map (source: source.url) asset.sources;
        # Nix accepts the manifest's SHA-256 hex directly; the fetch is a
        # fixed-output derivation independent of the host and model weights.
        sha256 = asset.sha256;
        name = asset.filename;
      };
    })
    manifest.assets;
in
assert manifest.schema_version == 1;
pkgs.linkFarm "maple-tokenizer-assets-v1" assets
