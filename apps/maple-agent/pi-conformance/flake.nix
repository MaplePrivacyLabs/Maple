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
              mkdir -p "$out/upstream"
              run_package_tests() {
                local package="$1"
                shift
                (
                  cd "packages/$package"
                  node "$PI_VITEST_CLI" list "$@" --includeTaskLocation \
                    --json="$out/upstream/$package-list.json"
                  test -s "$out/upstream/$package-list.json"
                  node "$PI_VITEST_CLI" run "$@" --includeTaskLocation \
                    --reporter=json --outputFile="$out/upstream/$package-results.json"
                  test -s "$out/upstream/$package-results.json"
                )
              }
              run_package_tests agent test/agent-loop.test.ts test/agent.test.ts
              run_package_tests coding-agent test/session-context-edit.test.ts test/extensions-runner.test.ts
            '';
          };

          corpus = mkReference {
            name = "corpus";
            script = ''
              cp -R ${./recorder} packages/coding-agent/test/maple-recorder
              chmod -R u+w packages/coding-agent/test/maple-recorder
              export PI_SCENARIOS_DIR=${./scenarios}
              for run in 1 2; do
                export PI_CORPUS_OUT="$TMPDIR/recording-$run"
                mkdir -p "$PI_CORPUS_OUT"
                (
                  cd packages/coding-agent
                  node "$PI_VITEST_CLI" run --config test/maple-recorder/vitest.config.ts \
                    test/maple-recorder/record.test.ts
                )
                test -n "$(find "$PI_CORPUS_OUT" -type f -print -quit)"
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
      });
    };
}
