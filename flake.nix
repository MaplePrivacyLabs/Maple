{
  description = "Reproducible Maple web, Tauri desktop, Android, and Apple build environments";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
          config = {
            allowUnfree = true;
            android_sdk.accept_license = true;
          };
        };
        lib = pkgs.lib;
        supportsAndroidHost = system == "x86_64-linux";

        versions = {
          bun = "1.3.5";
          rust = "1.94.1";
          jdk = "21";
          xcode = "26.5";
          android = rec {
            compileSdk = "36";
            platforms = [
              "34"
              "35"
              compileSdk
            ];
            buildTools = [
              "35.0.0"
              "34.0.0"
            ];
            ndk = "27.2.12479018";
          };
        };

        androidRustTargets = [
          "aarch64-linux-android"
          "armv7-linux-androideabi"
          "i686-linux-android"
          "x86_64-linux-android"
        ];
        appleRustTargets = [
          "aarch64-apple-darwin"
          "x86_64-apple-darwin"
          "aarch64-apple-ios"
          "aarch64-apple-ios-sim"
        ];
        hostRustTarget =
          if pkgs.stdenv.hostPlatform.config == "arm64-apple-darwin" then
            "aarch64-apple-darwin"
          else
            pkgs.stdenv.hostPlatform.config;
        nixRustTargets = lib.unique (
          [ hostRustTarget ]
          ++ lib.optionals supportsAndroidHost androidRustTargets
          ++ lib.optionals pkgs.stdenv.isDarwin appleRustTargets
        );

        rustToolchain = pkgs.rust-bin.stable.${versions.rust}.default.override {
          extensions = [
            "rust-src"
            "rustfmt"
            "clippy"
          ];
          targets =
            lib.optionals supportsAndroidHost androidRustTargets
            ++ lib.optionals pkgs.stdenv.isDarwin appleRustTargets;
        };

        rustupShim = pkgs.writeShellScriptBin "rustup" ''
          set -euo pipefail

          target_list="''${MAPLE_NIX_RUST_TARGETS:-}"

          list_installed() {
            local target
            for target in ''${target_list}; do
              printf '%s\n' "''${target}"
            done
          }

          has_target() {
            local requested="''${1:?target is required}"
            local target
            for target in ''${target_list}; do
              if [ "''${target}" = "''${requested}" ]; then
                return 0
              fi
            done
            return 1
          }

          case "''${1:-}" in
            target)
              shift
              case "''${1:-}" in
                list)
                  shift
                  if [ "''${1:-}" = "--installed" ]; then
                    list_installed
                    exit 0
                  fi

                  list_installed | while IFS= read -r target; do
                    printf '%s (installed)\n' "''${target}"
                  done
                  exit 0
                  ;;
                add)
                  shift
                  requested_targets=()
                  while [ "''${#}" -gt 0 ]; do
                    case "''${1}" in
                      --toolchain | -t)
                        shift
                        if [ "''${#}" -eq 0 ]; then
                          echo "rustup is shimmed by the Nix shell and --toolchain requires a value." >&2
                          exit 1
                        fi
                        shift
                        ;;
                      --*)
                        shift
                        ;;
                      *)
                        requested_targets+=("''${1}")
                        shift
                        ;;
                    esac
                  done

                  for target in "''${requested_targets[@]}"; do
                    if ! has_target "''${target}"; then
                      echo "rustup is shimmed by the Nix shell and target ''${target} is not provided by this flake." >&2
                      exit 1
                    fi
                  done
                  exit 0
                  ;;
              esac
              ;;
            show)
              if [ "''${2:-}" = "active-toolchain" ]; then
                printf 'nix-%s (default)\n' "''${MAPLE_NIX_RUST_VERSION:-unknown}"
                exit 0
              fi
              ;;
          esac

          echo "rustup is shimmed by the Maple Nix shell; use rustc/cargo from the flake toolchain." >&2
          exit 1
        '';

        jdk = pkgs.jdk21;

        commonPackages = with pkgs; [
          bash
          bun
          coreutils
          curl
          file
          findutils
          gawk
          git
          gnumake
          gnused
          gnutar
          gzip
          jq
          just
          minisign
          openssl
          p7zip
          pkg-config
          python3
          sccache
          unzip
          yq-go
          zip
          actionlint
          rustToolchain
        ];
        # Frontend scripts run tsc and Vite through their Node shebangs; pin
        # that runtime instead of relying on a hosted runner's global Node.
        ciPackages = [ rustupShim pkgs.nodejs ] ++ commonPackages;

        linuxTauriPackages =
          with pkgs;
          lib.optionals stdenv.isLinux [
            atk
            bubblewrap
            cairo
            gdk-pixbuf
            glib
            glib-networking
            gst_all_1.gstreamer
            gst_all_1.gst-libav
            gst_all_1.gst-plugins-bad
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gtk3
            librsvg
            libsoup_3
            mesa
            pango
            patchelf
            desktop-file-utils
            rpm
            squashfsTools
            webkitgtk_4_1
            xdg-utils
          ];

        linuxdeploySupportPackages =
          with pkgs;
          lib.optionals stdenv.isLinux (
            [
              bash
              binutils
              coreutils
              desktop-file-utils
              diffutils
              file
              findutils
              gawk
              gdk-pixbuf
              gdk-pixbuf.dev
              glib
              glib.dev
              glibc.bin
              gnugrep
              gnused
              gnutar
              gzip
              gtk3
              patchelf
              pkg-config
              squashfsTools
              util-linux
              which
              xdg-utils
            ]
            ++ linuxTauriPackages
          );

        linuxRuntimeClosure =
          if pkgs.stdenv.isLinux then
            pkgs.closureInfo {
              rootPaths = linuxTauriPackages ++ [ pkgs.stdenv.cc.cc.lib ];
            }
          else
            null;

        linuxTauriToolsArch =
          if pkgs.stdenv.hostPlatform.isx86_64 then
            "x86_64"
          else if pkgs.stdenv.hostPlatform.isAarch64 then
            "aarch64"
          else
            null;

        linuxTauriToolHashes = {
          appRun = {
            aarch64 = "sha256-By8XwIlahcSQKC/lOVxQB+X8ddpyflU7O4+2gP6xFXg=";
            x86_64 = "sha256-8wFApDoKWeRtshve/fdJuenyxpRukq+rus+YuK5z+08=";
          };
          linuxdeploy = {
            aarch64 = "sha256-sStcxXvQkh4fmNc/WKo2RQO8Gif1S3pp/ShwvOf6L1U=";
            x86_64 = "sha256-52K+qFyOsNSzUI1G5cHwN/cX0PkwOuO0qvyLBJkfoe8=";
          };
          appimagePlugin = {
            aarch64 = "sha256-g8KSFJJ0llqGXc1EwTXPyouijGt94+tijUuLXySK8Xw=";
            x86_64 = "sha256-mS1QKiSOFKsYVEjd9vbn0lVYy4TUYjw1TDrzUMJfzLM=";
          };
          appimageRuntime = {
            aarch64 = "sha256-AMvfz5F8xsD/bTNH1Z4Moff0Wm3xpCig1tinhmTYdEQ=";
            x86_64 = "sha256-L8qLRDySUQ8Ug6iD9gBhrQm0a5eLJjHIB82HOkfsJg0=";
          };
          gtkPlugin = "sha256-yzefmwcz6a2fi9ePjC+gOK7yR4Uju31MjmT/ah6jUBo=";
          gstreamerPlugin = "sha256-wQe0nYTtv/xqsibtEAfgYmpPeqLDo2t3gr72I1HUnpQ=";
        };

        tauriLinuxdeployTools =
          if pkgs.stdenv.isLinux && linuxTauriToolsArch != null then
            let
              arch = linuxTauriToolsArch;
              linuxdeployArch = if arch == "i686" then "i386" else arch;
              appRun = pkgs.fetchurl {
                url = "https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-${arch}";
                hash = linuxTauriToolHashes.appRun.${arch};
              };
              linuxdeploy = pkgs.fetchurl {
                url = "https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-${linuxdeployArch}.AppImage";
                hash = linuxTauriToolHashes.linuxdeploy.${linuxdeployArch};
              };
              appimagePlugin = pkgs.fetchurl {
                url = "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/1-alpha-20250213-1/linuxdeploy-plugin-appimage-${arch}.AppImage";
                hash = linuxTauriToolHashes.appimagePlugin.${arch};
              };
              appimageRuntime = pkgs.fetchurl {
                url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-${arch}";
                hash = linuxTauriToolHashes.appimageRuntime.${arch};
              };
              gtkPlugin = pkgs.fetchurl {
                url = "https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/b5eb8d05b4c0ed40107fe2158c5d8527f94568ef/linuxdeploy-plugin-gtk.sh";
                hash = linuxTauriToolHashes.gtkPlugin;
              };
              gstreamerPlugin = pkgs.fetchurl {
                url = "https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gstreamer/2a2e67491c32995a3f279ad0ecbe77abd512b42a/linuxdeploy-plugin-gstreamer.sh";
                hash = linuxTauriToolHashes.gstreamerPlugin;
              };
              linuxdeployWrapperSource = pkgs.writeText "maple-linuxdeploy-wrapper.c" ''
                #include <errno.h>
                #include <stdio.h>
                #include <stdlib.h>
                #include <string.h>
                #include <unistd.h>

                #ifndef LINUXDEPLOY_ARCH
                #error "LINUXDEPLOY_ARCH is required"
                #endif

                static char *wrapper_dir(const char *argv0) {
                  const char *slash = strrchr(argv0, '/');

                  if (slash == NULL) {
                    char *cwd = getcwd(NULL, 0);
                    if (cwd == NULL) {
                      perror("getcwd");
                    }
                    return cwd;
                  }

                  size_t len = (size_t)(slash - argv0);
                  if (len == 0) {
                    len = 1;
                  }

                  char *dir = malloc(len + 1);
                  if (dir == NULL) {
                    perror("malloc");
                    return NULL;
                  }

                  memcpy(dir, argv0, len);
                  dir[len] = '\0';
                  return dir;
                }

                int main(int argc, char **argv) {
                  char *dir = wrapper_dir(argv[0]);
                  if (dir == NULL) {
                    return 127;
                  }

                  char app_dir[8192];
                  int written = snprintf(
                    app_dir,
                    sizeof(app_dir),
                    "%s/linuxdeploy-%s.AppDir",
                    dir,
                    LINUXDEPLOY_ARCH
                  );

                  if (written < 0 || (size_t)written >= sizeof(app_dir)) {
                    free(dir);
                    fprintf(stderr, "linuxdeploy AppDir path is too long\n");
                    return 127;
                  }

                  char app_run[8192];
                  written = snprintf(app_run, sizeof(app_run), "%s/AppRun", app_dir);
                  if (written < 0 || (size_t)written >= sizeof(app_run)) {
                    free(dir);
                    fprintf(stderr, "linuxdeploy AppRun path is too long\n");
                    return 127;
                  }

                  char plugin_path[16384];
                  char plugin_bin[8192];
                  char support_bin[8192];
                  written = snprintf(
                    plugin_bin,
                    sizeof(plugin_bin),
                    "%s/maple-linuxdeploy-tools/plugins",
                    dir
                  );
                  if (written < 0 || (size_t)written >= sizeof(plugin_bin)) {
                    free(dir);
                    fprintf(stderr, "linuxdeploy plugin bin path is too long\n");
                    return 127;
                  }

                  written = snprintf(
                    support_bin,
                    sizeof(support_bin),
                    "%s/maple-linuxdeploy-tools/bin",
                    dir
                  );
                  if (written < 0 || (size_t)written >= sizeof(support_bin)) {
                    free(dir);
                    fprintf(stderr, "linuxdeploy support bin path is too long\n");
                    return 127;
                  }

                  written = snprintf(
                    plugin_path,
                    sizeof(plugin_path),
                    "%s:%s",
                    plugin_bin,
                    support_bin
                  );
                  if (written < 0 || (size_t)written >= sizeof(plugin_path)) {
                    free(dir);
                    fprintf(stderr, "linuxdeploy PATH is too long\n");
                    return 127;
                  }

                  free(dir);

                  if (setenv("APPDIR", app_dir, 1) != 0) {
                    perror("setenv APPDIR");
                    return 127;
                  }

                  if (setenv("PATH", plugin_path, 1) != 0) {
                    perror("setenv PATH");
                    return 127;
                  }

                  unsetenv("APPIMAGE");
                  unsetenv("APPIMAGE_EXTRACT_AND_RUN");
                  unsetenv("ARGV0");

                  char **args = calloc((size_t)argc + 1, sizeof(char *));
                  if (args == NULL) {
                    perror("calloc");
                    return 127;
                  }

                  int out = 0;
                  args[out++] = app_run;
                  for (int i = 1; i < argc; i++) {
                    if (strcmp(argv[i], "--appimage-extract-and-run") == 0) {
                      continue;
                    }
                    args[out++] = argv[i];
                  }
                  args[out] = NULL;

                  execv(app_run, args);
                  fprintf(stderr, "failed to exec %s: %s\n", app_run, strerror(errno));
                  return 127;
                }
              '';
            in
            pkgs.runCommand "maple-tauri-linuxdeploy-tools-${arch}" { } ''
              mkdir -p "$out"
              ${pkgs.stdenv.cc}/bin/cc -O2 -Wall -Wextra \
                -DLINUXDEPLOY_ARCH='"${linuxdeployArch}"' \
                ${linuxdeployWrapperSource} \
                -o "$out/linuxdeploy-${linuxdeployArch}.wrapper"
              install -m 0755 ${appRun} "$out/AppRun-${arch}"
              install -m 0755 ${linuxdeploy} "$out/linuxdeploy-${linuxdeployArch}.AppImage"
              install -m 0755 ${appimagePlugin} "$out/linuxdeploy-plugin-appimage.real.AppImage"
              install -m 0755 ${appimageRuntime} "$out/appimage-runtime-${arch}"
              install -m 0755 ${gtkPlugin} "$out/linuxdeploy-plugin-gtk.sh"
              install -m 0755 ${gstreamerPlugin} "$out/linuxdeploy-plugin-gstreamer.sh"
            ''
          else
            null;

        gstreamerPlugins =
          pkgs.symlinkJoin {
            name = "maple-gstreamer-plugins";
            paths = with pkgs.gst_all_1; [
              gstreamer.out
              gst-libav
              gst-plugins-bad
              gst-plugins-base
              gst-plugins-good
            ];
          };

        darwinPackages =
          with pkgs;
          lib.optionals stdenv.isDarwin [
            cmake
            libiconv
            python3
          ];

        androidComposition =
          if supportsAndroidHost then
            pkgs.androidenv.composeAndroidPackages {
              platformVersions = versions.android.platforms;
              buildToolsVersions = versions.android.buildTools;
              ndkVersions = [ versions.android.ndk ];
              includeNDK = true;
              includeEmulator = false;
              includeSystemImages = false;
              abiVersions = [
                "arm64-v8a"
                "armeabi-v7a"
                "x86"
                "x86_64"
              ];
            }
          else
            null;

        androidPackages = lib.optionals pkgs.stdenv.isLinux [
          pkgs.apksigner
          jdk
        ] ++ lib.optionals supportsAndroidHost [ androidComposition.androidsdk ];

        mkShellForHost = if pkgs.stdenv.isDarwin && pkgs ? mkShellNoCC then pkgs.mkShellNoCC else pkgs.mkShell;

        linuxShellHook = lib.optionalString pkgs.stdenv.isLinux ''
          export MAPLE_NIX_CC=${pkgs.stdenv.cc}/bin/cc
          export MAPLE_NIX_GCC_LIB=${pkgs.stdenv.cc.cc.lib}
          export MAPLE_NIX_GDK_PIXBUF_BINARYDIR=${pkgs.gdk-pixbuf}/lib/gdk-pixbuf-2.0/2.10.0
          export MAPLE_NIX_GDK_PIXBUF_MODULEDIR=${pkgs.gdk-pixbuf}/lib/gdk-pixbuf-2.0/2.10.0/loaders
          export MAPLE_NIX_GLIB_SCHEMAS=${pkgs.glib.dev}/share/glib-2.0/schemas
          export MAPLE_NIX_GTK_LIB=${pkgs.gtk3}/lib
          export MAPLE_NIX_LINUX_CLOSURE_INFO=${linuxRuntimeClosure}
          export MAPLE_NIX_LINUXDEPLOY_SUPPORT_PATH=${lib.makeBinPath linuxdeploySupportPackages}
          export GSTREAMER_PLUGINS_DIR=${gstreamerPlugins}/lib/gstreamer-1.0
          export GSTREAMER_HELPERS_DIR=${pkgs.gst_all_1.gstreamer.out}/libexec/gstreamer-1.0
          export __EGL_VENDOR_LIBRARY_FILENAMES=${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json
          export LIBGL_DRIVERS_PATH=${pkgs.mesa}/lib/dri
          export LIBGL_ALWAYS_SOFTWARE=1
          export WEBKIT_DISABLE_COMPOSITING_MODE=1
          export WEBKIT_DISABLE_DMABUF_RENDERER=1
          export GIO_MODULE_DIR=${pkgs.glib-networking}/lib/gio/modules
        '';

        linuxDesktopShellHook = lib.optionalString pkgs.stdenv.isLinux ''
          ${lib.optionalString (tauriLinuxdeployTools != null) "export MAPLE_NIX_TAURI_LINUXDEPLOY_TOOLS=${tauriLinuxdeployTools}"}
          ${lib.optionalString (linuxTauriToolsArch != null) "export MAPLE_NIX_TAURI_LINUXDEPLOY_ARCH=${linuxTauriToolsArch}"}
        '';

        androidShellHook = lib.optionalString supportsAndroidHost ''
          export JAVA_HOME=${jdk}
          export ANDROID_HOME=${androidComposition.androidsdk}/libexec/android-sdk
          export ANDROID_SDK_ROOT="$ANDROID_HOME"

          if [ -d "$ANDROID_HOME/ndk/${versions.android.ndk}" ]; then
            export NDK_HOME="$ANDROID_HOME/ndk/${versions.android.ndk}"
          elif [ -d "$ANDROID_HOME/ndk-bundle" ]; then
            export NDK_HOME="$ANDROID_HOME/ndk-bundle"
          fi
        '' + lib.optionalString (pkgs.stdenv.isLinux && !supportsAndroidHost) ''
          export MAPLE_ANDROID_UNSUPPORTED_HOST=1
          echo "Android PR builds require x86_64-linux; current Nix system is ${system}."
        '';

        commonShellHook = ''
          export MAPLE_NIX_BUN_VERSION=${versions.bun}
          export MAPLE_NIX_RUST_VERSION=${versions.rust}
          export MAPLE_NIX_RUST_TARGETS="${lib.concatStringsSep " " nixRustTargets}"
          export MAPLE_NIX_JDK_VERSION=${versions.jdk}
          export MAPLE_NIX_XCODE_VERSION=${versions.xcode}
          export MAPLE_NIX_GNUTAR=${pkgs.gnutar}/bin/tar
          export MAPLE_NIX_GZIP=${pkgs.gzip}/bin/gzip
          ${lib.optionalString pkgs.stdenv.isDarwin "export MAPLE_NIX_LIBICONV=${pkgs.libiconv}"}
          export CARGO_TERM_COLOR=always
          export RUST_BACKTRACE=1
          if [ -z "''${CI:-}" ] \
            && [ "''${MAPLE_DISABLE_SHARED_CARGO_BUILD_DIR:-0}" != "1" ] \
            && [ -z "''${CARGO_BUILD_BUILD_DIR:-}" ]; then
            export CARGO_BUILD_BUILD_DIR="$HOME/.cache/opensecret-workspaces/cargo-build/maple/${system}/rust-${versions.rust}"
          fi
          echo "Maple Nix toolchain: bun $(bun --version), $(rustc --version)"
          if [ -n "''${CARGO_BUILD_BUILD_DIR:-}" ]; then
            echo "Maple Cargo build cache: $CARGO_BUILD_BUILD_DIR"
          fi
        '';

        pathShellHook = packages: ''
          export PATH="${lib.makeBinPath packages}:$PATH"
        '';

        mkNixApp =
          name: shell: script:
          (flake-utils.lib.mkApp {
            drv = pkgs.writeShellApplication {
              inherit name;
              runtimeInputs = [ pkgs.nix ];
              text = ''
                exec nix develop --no-update-lock-file ".#${shell}" -c ${script} "$@"
              '';
            };
          }) // {
            meta.description = "Run ${script} through nix develop --no-update-lock-file .#${shell}";
          };

        desktopCiShell = if pkgs.stdenv.isLinux then "desktop-linux" else "ci";
      in
      {
        devShells = {
          default =
            let
              shellPackages = commonPackages ++ [ jdk ] ++ linuxTauriPackages ++ darwinPackages;
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages + commonShellHook + linuxShellHook + linuxDesktopShellHook;
            };

          ci =
            let
              shellPackages = ciPackages ++ [ jdk ] ++ linuxTauriPackages ++ darwinPackages;
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages + commonShellHook + linuxShellHook;
            };

          # Trusted Pages publishing uploads static artifacts. It needs no app
          # toolchain, native setup hooks, or credentials while installing Wrangler.
          pages =
            let
              shellPackages = with pkgs; [ bun nodejs python3 cacert git ];
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages;
            };
        }
        // lib.optionalAttrs pkgs.stdenv.isLinux {
          desktop-linux =
            let
              shellPackages = ciPackages ++ [ jdk ] ++ linuxTauriPackages;
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages + commonShellHook + linuxShellHook + linuxDesktopShellHook;
            };

          android =
            let
              shellPackages = ciPackages ++ androidPackages;
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages + commonShellHook + androidShellHook;
            };
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          apple =
            let
              shellPackages = ciPackages ++ [ jdk ] ++ darwinPackages;
            in
            mkShellForHost {
              packages = shellPackages;
              shellHook = pathShellHook shellPackages + commonShellHook;
            };
        };

        checks = {
          toolchain = pkgs.runCommand "maple-toolchain-check" { nativeBuildInputs = commonPackages ++ [ jdk ]; } ''
            test "$(bun --version)" = "${versions.bun}"
            rustc --version | grep -F "rustc ${versions.rust}"
            java -version 2>&1 | grep -F 'version "${versions.jdk}.'
            touch "$out"
          '';

          workflows = pkgs.runCommand "maple-github-workflows-check" {
            nativeBuildInputs = with pkgs; [ actionlint python3 yq-go ];
            src = ./.github/workflows;
            tests = ./scripts/ci/test_codeql_workflow.py;
            signingTests = ./scripts/ci/test_signing_workflows.py;
          } ''
            cd "$src"
            # actionlint 1.7.10 predates GitHub's supported concurrency.queue key.
            # Release-gate tests assert that the only intended value is queue: max.
            for workflow in ./*.yml; do
              if [ "$workflow" = ./pages-publish.yml ] || [ "$workflow" = ./auth-pages-publish.yml ]; then
                # It also predates environment.deployment. Pages tests require
                # false on the publisher jobs; keep this exception file-scoped.
                actionlint -config-file ${./.github/actionlint.yaml} -ignore 'unexpected key "deployment" for "environment" section' "$workflow"
              else
                actionlint -config-file ${./.github/actionlint.yaml} -ignore 'unexpected key "queue" for "concurrency" section' "$workflow"
              fi
            done
            mkdir -p "$TMPDIR/repo/.github/workflows" "$TMPDIR/repo/scripts/ci"
            cp -R . "$TMPDIR/repo/.github/workflows/"
            cp "$tests" "$TMPDIR/repo/scripts/ci/test_codeql_workflow.py"
            python3 "$TMPDIR/repo/scripts/ci/test_codeql_workflow.py"
            cp "$signingTests" "$TMPDIR/repo/scripts/ci/test_signing_workflows.py"
            python3 "$TMPDIR/repo/scripts/ci/test_signing_workflows.py"
            touch "$out"
          '';

          change-detection = pkgs.runCommand "maple-ci-change-detection-check" {
            nativeBuildInputs = [ pkgs.python3 ];
            src = ./.;
          } ''
            cd "$src"
            python3 scripts/ci/test_change_detection.py
            python3 scripts/ci/test_agent_change_detection.py
            python3 scripts/ci/test_opensecret_change_detection.py
            python3 scripts/ci/test_hook_change_detection.py
            touch "$out"
          '';

          opensecret-workflows = pkgs.runCommand "maple-opensecret-workflow-check" {
            nativeBuildInputs = with pkgs; [ bash git jq python3 yq-go ];
            src = ./.;
          } ''
            cd "$src"
            python3 scripts/ci/test_opensecret_workflows.py
            touch "$out"
          '';

          pcr-compatibility = pkgs.runCommand "maple-pcr-compatibility-check" {
            nativeBuildInputs = [ pkgs.git (pkgs.python3.withPackages (ps: [ ps.cryptography ])) ];
            src = ./.;
          } ''
            cd "$src"
            python3 services/opensecret/scripts/test_pcr_compatibility.py
            python3 services/opensecret/scripts/pcr_compatibility.py check services/opensecret
            touch "$out"
          '';

          agent-workflows = pkgs.runCommand "maple-agent-workflow-check" {
            nativeBuildInputs = with pkgs; [ bash git python3 yq-go ];
            src = ./.;
          } ''
            cd "$src"
            python3 scripts/ci/test_agent_workflows.py
            python3 scripts/ci/test_agent_rust_deps.py
            python3 scripts/ci/test_agent_debug_plist.py
            touch "$out"
          '';

          ios-build-profile = pkgs.runCommand "maple-ios-build-profile-check" {
            nativeBuildInputs = with pkgs; [ bash python3 ];
            src = ./.;
          } ''
            cd "$src"
            python3 scripts/ci/test_ios_build_profile.py
            touch "$out"
          '';

          pages = pkgs.runCommand "maple-pages-deployment-check" {
            nativeBuildInputs = with pkgs; [ bash python3 yq-go ];
            src = ./.;
          } ''
            cd "$src"
            python3 -I -m unittest discover -s scripts/ci -p 'test_pages_*.py'
            touch "$out"
          '';

          sdk-publishing = pkgs.runCommand "maple-sdk-publishing-check" {
            nativeBuildInputs = with pkgs; [ bash git python3 yq-go ];
            src = ./.;
          } ''
            cd "$src"
            python3 -I -m unittest discover -s scripts/ci -p 'test_sdk_publish*.py'
            python3 -I -m unittest discover -s scripts/ci -p 'test_sdk_crates_upload.py'
            touch "$out"
          '';

          release-metadata = pkgs.runCommand "maple-release-metadata-check" {
            nativeBuildInputs = commonPackages;
            src = ./.;
          } ''
            cd "$src"
            bash ./scripts/ci/validate-release-version.sh >/dev/null
            touch "$out"
          '';

          release-gates = pkgs.runCommand "maple-release-gates-check" {
            nativeBuildInputs = with pkgs; [
              bash
              coreutils
              git
              jq
              python3
              ripgrep
              gnutar
              unzip
              zip
              yq-go
            ];
            src = ./.;
          } ''
            cd "$src"
            bash ./scripts/ci/test-release-gates.sh
            touch "$out"
          '';
        };

        apps = {
          ci-frontend = mkNixApp "maple-ci-frontend" "ci" "./scripts/ci/frontend.sh";
          ci-rust = mkNixApp "maple-ci-rust" "ci" "./scripts/ci/rust.sh";
          ci-desktop-pr = mkNixApp "maple-ci-desktop-pr" desktopCiShell "./scripts/ci/desktop-pr.sh";
          ci-desktop-release = mkNixApp "maple-ci-desktop-release" desktopCiShell "./scripts/ci/desktop-release.sh";
          ci-signed-desktop-release-rehearsal = mkNixApp "maple-ci-signed-desktop-release-rehearsal" desktopCiShell "./scripts/ci/signed-release-rehearsal.sh desktop";
          ci-latest-json = mkNixApp "maple-ci-latest-json" "ci" "./scripts/ci/latest-json.sh";
          ci-verify-release-artifacts = mkNixApp "maple-ci-verify-release-artifacts" "ci" "./scripts/ci/verify-release-artifacts.sh";
        }
        // lib.optionalAttrs pkgs.stdenv.isLinux {
          ci-android-pr = mkNixApp "maple-ci-android-pr" "android" "./scripts/ci/android-pr.sh";
          ci-android-release = mkNixApp "maple-ci-android-release" "android" "./scripts/ci/android-release.sh";
          ci-signed-android-release-rehearsal = mkNixApp "maple-ci-signed-android-release-rehearsal" "android" "./scripts/ci/signed-release-rehearsal.sh android";
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          ci-ios-onnxruntime = mkNixApp "maple-ci-ios-onnxruntime" "apple" "./scripts/ci/ios-onnxruntime.sh";
          ci-ios-pr = mkNixApp "maple-ci-ios-pr" "apple" "./scripts/ci/ios-pr.sh";
          ci-ios-release = mkNixApp "maple-ci-ios-release" "apple" "./scripts/ci/ios-release.sh";
          ci-signed-ios-release-rehearsal = mkNixApp "maple-ci-signed-ios-release-rehearsal" "apple" "./scripts/ci/signed-release-rehearsal.sh ios";
        };
      }
    );
}
