{
  description = "Pinned, offline TypeScript reference for the Pi Rust port";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/3ed67ec0a4d3c7ab4ae1f04f8ee8df07bfa506a2";
    pi = {
      url = "github:earendil-works/pi/v1.0.4";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      pi,
    }:
    let
      pin = builtins.fromJSON (builtins.readFile ./pin.json);
      harnessInputs = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions (
          [
            ./flake.nix
            ./flake.lock
            ./pin.json
            ./nix
            ./recorder
            ./scenarios
            ./selection
          ]
          ++ nixpkgs.lib.optional (builtins.pathExists ./fixtures) ./fixtures
        );
      };
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    assert pi.rev == pin.rev;
    assert nixpkgs.rev == pin.nixpkgs;
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          mkReference =
            args:
            pkgs.callPackage ./nix/pi-reference.nix (
              {
                source = pi;
                inherit pin;
              }
              // args
            );
        in
        rec {
          upstream-tests = mkReference {
            name = "upstream-tests";
            script = ''
              python3 -I -B ${./selection}/generate-test-files.py --check --upstream-root "$PWD"
              mkdir -p "$out/upstream"
              mkdir -p .maple-harness
              cp ${./recorder/inventory.ts} .maple-harness/inventory.ts
              export PI_SELECTION_DIR=${./selection}
              export PI_PIN_JSON=${./pin.json}
              for package in agent ai coding-agent; do
                node .maple-harness/inventory.ts collect "$package" "$out/upstream/$package.inventory.json"
                node .maple-harness/inventory.ts run "$package" "$out/upstream/$package.results.json"
              done
              node .maple-harness/inventory.ts combine "$out/upstream"
              node .maple-harness/inventory.ts coverage "$out/upstream/inventory.json" "$out/upstream-map.toml"
            '';
          };

          corpus = mkReference {
            name = "corpus";
            script = ''
              cp -R ${./recorder} packages/coding-agent/test/maple-recorder
              chmod -R u+w packages/coding-agent/test/maple-recorder
              export PI_SCENARIOS_DIR=${./scenarios}
              export PI_HARNESS_INPUTS=${harnessInputs}
              export PI_UPSTREAM_RESULTS=${upstream-tests}/upstream
              for run in 1 2; do
                export PI_CORPUS_OUT="$TMPDIR/recording-$run"
                mkdir -p "$PI_CORPUS_OUT"
                (
                  cd packages/coding-agent
                  node "$PI_VITEST_CLI" run --config test/maple-recorder/vitest.config.ts \
                    test/maple-recorder/record.test.ts
                )
                test -n "$(find "$PI_CORPUS_OUT" -type f -print -quit)"
                node packages/coding-agent/test/maple-recorder/build-corpus.ts "$PI_CORPUS_OUT"
              done
              diff -ru "$TMPDIR/recording-1" "$TMPDIR/recording-2"
              cp -R "$TMPDIR/recording-1" "$out/corpus"
            '';
          };

          spike = pkgs.runCommand "pi-reference-spike-${pin.tag}" { } ''
            mkdir -p "$out"
            cp -R ${upstream-tests}/upstream "$out/upstream"
            cp -R ${corpus}/corpus "$out/corpus"
            cp ${corpus}/node-version.txt "$out/node-version.txt"
            cp ${corpus}/catalog-bytes.txt "$out/catalog-bytes.txt"
          '';

          default = spike;
        }
      );

      checks = forAllSystems (system: {
        spike = self.packages.${system}.spike;
        corpus-fresh = (import nixpkgs { inherit system; }).runCommand "pi-corpus-fresh" { } ''
          diff -ru ${self.packages.${system}.corpus}/corpus ${./corpus}
          touch "$out"
        '';
      });
    };
}
