{
  description = "Native Maple desktop app built with GPUI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      # The pinned Swift compiler cannot compile the macOS CUA bridges with
      # the SDK required for recording. macOS builds use the Xcode dev shell.
      packageSystems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      forPackageSystems = nixpkgs.lib.genAttrs packageSystems;
      # The store reference in runtime.json retains exactly the declared CPython
      # and its stdlib/native dependency closure in the installed Maple output.
      mkPythonRuntime =
        pkgs:
        let
          python = pkgs.python313;
          version = python.version;
          manifest = {
            protocol_version = 1;
            implementation = "cpython";
            inherit version;
            distribution = "nix-${python.name}-${builtins.baseNameOf (toString python)}";
            executable = "${python}/bin/python3.13";
            worker = "worker.py";
          };
        in
        assert version == "3.13.15";
        pkgs.runCommand "maple-python-${version}" { nativeBuildInputs = [ python ]; } ''
          runtime="$out/share/maple-gpui/python"
          mkdir -p "$runtime/licenses"
          cp ${./crates/maple-code-mode/python/worker.py} "$runtime/worker.py"
          cat > "$runtime/runtime.json" <<'JSON'
          ${builtins.toJSON manifest}
          JSON
          tar -xOf ${python.src} Python-${version}/LICENSE > "$runtime/licenses/Python-LICENSE.txt"
          ${python}/bin/python3.13 -I -B -c 'import sys, sysconfig, ssl, sqlite3, ctypes, zlib, bz2, lzma; assert sys.version_info[:3] == (3, 13, 15); assert not sysconfig.get_config_var("Py_GIL_DISABLED")'
        '';
    in
    {
      packages = forPackageSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "clippy"
              "rustfmt"
            ];
          };
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
          pythonRuntime = mkPythonRuntime pkgs;
          linuxRuntimeInputs = with pkgs; [
            libxcb
            libxkbcommon
            mesa
            vulkan-loader
            wayland
            # Embedded CUA drives X11 windows and synthetic input through the
            # Cua Driver SDK's Linux backend. Wayland sessions reach the
            # desktop through the portal and libei, which are pure Rust.
            xorg.libX11
            xorg.libXi
            xorg.libXtst
          ];
          linuxBuildInputs =
            with pkgs;
            [
              alsa-lib
              fontconfig
              freetype
            ]
            ++ linuxRuntimeInputs;
        in
        {
          python-runtime = pythonRuntime;
          default = rustPlatform.buildRustPackage {
            pname = "maple-gpui";
            version = "0.1.0";

            src = pkgs.lib.fileset.toSource {
              root = ../..;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.lock
                ./Cargo.toml
                ./app
                ./crates
                ../../sdk/rust/Cargo.toml
                ../../sdk/rust/src
                ../../sdk/rust/assets
                ../../proxy/Cargo.toml
                ../../proxy/src
              ];
            };

            # Keep the source tree rooted at Maple so sibling path dependencies
            # resolve identically in a pure package and a development checkout.
            cargoRoot = "apps/maple-agent";
            buildAndTestSubdir = "apps/maple-agent";

            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                "collections-0.1.0" = "sha256-d2GVmZgvJzLk1pbNtPedw0V09+ANZFORZjTSLVxw7jc=";
                "proptest-1.10.0" = "sha256-p5NTcHhruI8QQvANACg8AMRVNmuvGxs2NLit+/8PaWo=";
                "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
                "xim-ctext-0.3.0" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
                "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
                "zed-scap-0.0.8-zed" = "sha256-BihiQHlal/eRsktyf0GI3aSWsUCW7WcICMsC2Xvb7kw=";
                "cua-driver-sdk-0.23.2" = "sha256-769sMyUJU32sjNKQ0ozmwiEEh8rTHgu2ikqdoLrHuZE=";
                "goose-1.47.0" = "sha256-CmRMAMlCnfW4NdSREHO+HA2seWu1RkbLHxU0JhCS5XU=";
              };
            };

            nativeBuildInputs =
              with pkgs;
              [
                clang
                cmake
                pkg-config
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.makeWrapper ];

            buildInputs = [
              pkgs.libiconv
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux linuxBuildInputs;

            cargoBuildFlags = [
              "-p"
              "maple-gpui"
            ];

            # The upstream CI runs the complete workspace and feature matrix.
            # Keep the package derivation focused on producing the release binary.
            doCheck = false;

            postInstall = ''
              mkdir -p "$out/share/maple-gpui"
              cp -R ${pythonRuntime}/share/maple-gpui/python "$out/share/maple-gpui/python"
            '';

            # GPUI loads the Wayland and Vulkan libraries at runtime, so they are
            # not discovered by ELF dependency scanning. Prefer the NixOS GPU
            # driver link and retain Mesa as a portable fallback elsewhere.
            postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram "$out/bin/maple-gpui" \
                --prefix LD_LIBRARY_PATH : "${pkgs.addDriverRunpath.driverLink}/lib:${pkgs.lib.makeLibraryPath linuxRuntimeInputs}" \
                --suffix VK_ADD_DRIVER_FILES : "${pkgs.addDriverRunpath.driverLink}/share/vulkan/icd.d:${pkgs.mesa}/share/vulkan/icd.d"
            '';

            meta = {
              description = "Native Maple desktop app built with GPUI";
              homepage = "https://github.com/MaplePrivacyLabs/Maple/tree/master/apps/maple-agent";
              license = pkgs.lib.licenses.mit;
              mainProgram = "maple-gpui";
              platforms = packageSystems;
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "clippy"
              "rustfmt"
            ];
          };
          linuxRuntimeInputs = with pkgs; [
            libxcb
            libxkbcommon
            mesa
            vulkan-loader
            wayland
            # Embedded CUA drives X11 windows and synthetic input through the
            # Cua Driver SDK's Linux backend. Wayland sessions reach the
            # desktop through the portal and libei, which are pure Rust.
            xorg.libX11
            xorg.libXi
            xorg.libXtst
          ];
          linuxBuildInputs =
            with pkgs;
            [
              alsa-lib
              fontconfig
              freetype
            ]
            ++ linuxRuntimeInputs;
          isDarwin = pkgs.stdenv.hostPlatform.isDarwin;
          mkDevShell = if isDarwin then pkgs.mkShellNoCC else pkgs.mkShell;
          xcrun = pkgs.writeShellScriptBin "xcrun" ''
            exec /usr/bin/xcrun "$@"
          '';
        in
        {
          default = mkDevShell {
            packages =
              with pkgs;
              [
                clang
                cmake
                pkg-config
                rustToolchain
                python313
                just
              ]
              ++ pkgs.lib.optionals isDarwin [ xcrun ];

            buildInputs = [
              pkgs.libiconv
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux linuxBuildInputs;

            # ARM64 Linux is a Nix-only delivery target. Other development
            # platforms still prepare PBS unless Nix is selected explicitly.
            MAPLE_CODE_MODE_NIX_RUNTIME_MANIFEST = "${mkPythonRuntime pkgs}/share/maple-gpui/python/runtime.json";

            shellHook = ''
              if [ "${system}" = "aarch64-linux" ]; then
                export MAPLE_CODE_MODE_DISTRIBUTION="''${MAPLE_CODE_MODE_DISTRIBUTION:-nix}"
              fi
              if [ "''${MAPLE_CODE_MODE_DISTRIBUTION:-pbs}" = "nix" ]; then
                export MAPLE_CODE_MODE_RUNTIME_MANIFEST="''${MAPLE_CODE_MODE_RUNTIME_MANIFEST:-$MAPLE_CODE_MODE_NIX_RUNTIME_MANIFEST}"
              fi
              if [ -z "''${CI:-}" ] \
                && [ "''${MAPLE_GPUI_DISABLE_SHARED_CARGO_BUILD_DIR:-0}" != "1" ] \
                && [ -z "''${CARGO_BUILD_BUILD_DIR:-}" ] \
                && command -v rustc >/dev/null 2>&1; then
                maple_gpui_rust_host="$(rustc -vV | awk '/^host:/{print $2}')"
                maple_gpui_rust_version="$(rustc --version | awk '{print $2}')"
                export CARGO_BUILD_BUILD_DIR="$HOME/.cache/cargo-build/maple-gpui/''${maple_gpui_rust_host}/rust-''${maple_gpui_rust_version}"
                unset maple_gpui_rust_host maple_gpui_rust_version
              fi
              if [ -n "''${CARGO_BUILD_BUILD_DIR:-}" ]; then
                echo "maple-gpui Cargo build cache: $CARGO_BUILD_BUILD_DIR"
              fi
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              export LD_LIBRARY_PATH="${pkgs.addDriverRunpath.driverLink}/lib:${pkgs.lib.makeLibraryPath linuxRuntimeInputs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export VK_ADD_DRIVER_FILES="${pkgs.addDriverRunpath.driverLink}/share/vulkan/icd.d:${pkgs.mesa}/share/vulkan/icd.d''${VK_ADD_DRIVER_FILES:+:$VK_ADD_DRIVER_FILES}"
            ''
            + pkgs.lib.optionalString isDarwin ''
              maple_nix_valid_developer_dir() {
                [ -d "$1" ] \
                  && [ -x "$1/usr/bin/xcodebuild" ] \
                  && DEVELOPER_DIR="$1" /usr/bin/xcrun --sdk macosx --show-sdk-path >/dev/null 2>&1
              }

              maple_nix_developer_dir=""
              if [ -n "''${MAPLE_NIX_XCODE_VERSION:-}" ]; then
                maple_nix_developer_dir="/Applications/Xcode_''${MAPLE_NIX_XCODE_VERSION}.app/Contents/Developer"
                if ! maple_nix_valid_developer_dir "$maple_nix_developer_dir"; then
                  echo "Maple Nix shell: Xcode ''${MAPLE_NIX_XCODE_VERSION} was not found at $maple_nix_developer_dir." >&2
                  echo "Install that version, or unset MAPLE_NIX_XCODE_VERSION and set DEVELOPER_DIR to a full Xcode installation." >&2
                  exit 1
                fi
              elif [ -n "''${DEVELOPER_DIR:-}" ] && maple_nix_valid_developer_dir "$DEVELOPER_DIR"; then
                maple_nix_developer_dir="$DEVELOPER_DIR"
              elif maple_nix_valid_developer_dir "/Applications/Xcode.app/Contents/Developer"; then
                maple_nix_developer_dir="/Applications/Xcode.app/Contents/Developer"
              else
                maple_nix_selected_developer_dir="$(DEVELOPER_DIR= /usr/bin/xcode-select -p 2>/dev/null || true)"
                if maple_nix_valid_developer_dir "$maple_nix_selected_developer_dir"; then
                  maple_nix_developer_dir="$maple_nix_selected_developer_dir"
                else
                  echo "Maple Nix shell: no full Xcode installation was found." >&2
                  echo "Install Xcode, select it with xcode-select, or set MAPLE_NIX_XCODE_VERSION/DEVELOPER_DIR." >&2
                  exit 1
                fi
              fi

              export DEVELOPER_DIR="$maple_nix_developer_dir"
              export SDKROOT="$(/usr/bin/xcrun --sdk macosx --show-sdk-path)"
              # Native Rust dependencies such as aws-lc ask cc-rs to compile
              # against the macOS SDK. Point cc-rs at Xcode's actual compiler
              # so it recognizes the Apple toolchain and supplies -isysroot;
              # nixpkgs' generic clang wrapper cannot infer that SDK boundary.
              export CC="$(/usr/bin/xcrun --find clang)"
              export CXX="$(/usr/bin/xcrun --find clang++)"
              export PATH="${xcrun}/bin:$PATH"

              unset maple_nix_developer_dir maple_nix_selected_developer_dir
              unset -f maple_nix_valid_developer_dir
            '';
          };
        }
      );
    };
}
