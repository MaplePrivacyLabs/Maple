{ pkgs }:
let
  inherit (pkgs) lib;

  compiler =
    language:
    pkgs.writeShellScript "maple-portable-${language}" ''
      set -eu
      output=a.out
      next_output=false
      compile_only=false
      has_input=false
      for argument in "$@"; do
        if "$next_output"; then
          output="$argument"
          next_output=false
          continue
        fi
        case "$argument" in
          -o) next_output=true ;;
          -o?*) output="''${argument#-o}" ;;
          -c|-E|-S) compile_only=true ;;
          *) if [ -f "$argument" ] || [ "$argument" = - ]; then has_input=true; fi ;;
        esac
      done
      ${pkgs.zig}/bin/zig ${language} -target x86_64-linux-gnu.2.31 "$@"
      # Zig 0.16 cannot accept an explicit dynamic-linker flag while it builds
      # its own libc stubs. Patch only finished build-time executables instead;
      # objects and shared libraries carry no interpreter and are left alone.
      if "$has_input" && ! "$compile_only" && [ -f "$output" ] \
        && ${pkgs.patchelf}/bin/patchelf --print-interpreter "$output" >/dev/null 2>&1; then
        ${pkgs.patchelf}/bin/patchelf --set-interpreter \
          ${pkgs.glibc}/lib/ld-linux-x86-64.so.2 "$output"
      fi
    '';

  # Rebuild the small application-library closure from the owning flake's
  # current source pins. The ordinary Nix outputs target its newer glibc;
  # copying them into a portable bundle would silently raise our ABI floor.
  # stdenvNoCC deliberately supplies no Nix libc/compiler-wrapper flags.
  portable =
    package: dependencies:
    (package.override ({ stdenv = pkgs.stdenvNoCC; } // dependencies)).overrideAttrs (old: {
      # Refer to Zig by its immutable path rather than loading its setup hook,
      # which would replace Autotools/Meson's phases with `zig build`.
      nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.binutils ];
      env = (old.env or { }) // {
        CC = compiler "cc";
        CXX = compiler "c++";
        # Nix's Meson also prefers *_FOR_BUILD during native builds. Keep
        # generators and installed libraries on the same baseline compiler.
        CC_FOR_BUILD = compiler "cc";
        CXX_FOR_BUILD = compiler "c++";
        AR = "${pkgs.binutils}/bin/ar";
        RANLIB = "${pkgs.binutils}/bin/ranlib";
        CFLAGS = "-O2 -fPIC -fstack-protector-strong -D_FORTIFY_SOURCE=2";
        CXXFLAGS = "-O2 -fPIC -fstack-protector-strong -D_FORTIFY_SOURCE=2";
        # Configure probes/code generators run on Nix's libc. The compiler
        # wrapper patches their interpreter; packaging replaces library RPATHs.
        LDFLAGS = "-Wl,-z,relro,-z,now -Wl,-z,noexecstack -Wl,-rpath,${pkgs.glibc}/lib";
      };
      preConfigure = (old.preConfigure or "") + ''
        export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
      '';
      postInstall = (old.postInstall or "") + ''
        # Keep the upstream license alongside each bundled library. This is
        # source material, not a generated dependency-notice inventory.
        runtime_license_dir="$out/share/licenses/${old.pname}-${old.version}"
        mkdir -p "$runtime_license_dir"
        case "$sourceRoot" in
          /*) runtime_source_root="$sourceRoot" ;;
          *) runtime_source_root="$NIX_BUILD_TOP/$sourceRoot" ;;
        esac
        runtime_license_count=0
        while IFS= read -r -d $'\0' runtime_license; do
          cp "$runtime_license" "$runtime_license_dir/"
          runtime_license_count=$((runtime_license_count + 1))
        done < <(find "$runtime_source_root" -maxdepth 1 -type f \
          \( -iname 'copying*' -o -iname 'license*' -o -iname 'licence*' -o -iname 'copyright*' \) -print0)
        test "$runtime_license_count" -gt 0
        cat > "$runtime_license_dir/source.json" <<'MAPLE_RUNTIME_SOURCE'
        ${builtins.toJSON {
          name = old.pname;
          version = old.version;
          source = {
            urls = old.src.urls or (lib.optional (old.src ? url) old.src.url);
            hash = old.src.outputHash;
            hashMode = old.src.outputHashMode or "flat";
          };
        }}
        MAPLE_RUNTIME_SOURCE
      '';
      doCheck = false;
      doInstallCheck = false;
      separateDebugInfo = false;
    });

  runtime = rec {
    libxau = portable pkgs.libxau { };
    libxdmcp = portable pkgs.libxdmcp { };
    libxcb = portable pkgs.libxcb { inherit libxau libxdmcp; };
    libx11 = (portable pkgs.libx11 { inherit libxcb; }).overrideAttrs (old: {
      # Install into the Nix output, but read the receiving desktop's locale
      # data rather than retaining a build-store data directory in the DSO.
      postConfigure = (old.postConfigure or "") + ''
        substituteInPlace src/config.h \
          --replace-fail "$out/share/X11" /usr/share/X11
      '';
    });
    libxext = (portable pkgs.libxext { inherit libx11 libxau; }).overrideAttrs (old: {
      # XSecurity's Xauth.h dependency is a propagated Nix include, rather
      # than part of xext's pkg-config flags. Pass that header path explicitly
      # without importing the ordinary compiler wrapper's libc search paths.
      env = old.env // {
        CPPFLAGS = "-isystem ${lib.getDev libxau}/include";
      };
    });
    libxfixes = portable pkgs.libxfixes { inherit libx11; };
    libxi = portable pkgs.libxi { inherit libx11 libxext libxfixes; };
    libxtst = portable pkgs.libxtst { inherit libx11 libxext libxi; };
    libffi = portable pkgs.libffi { };
    wayland = portable (pkgs.wayland.override {
      withTests = false;
      withDocumentation = false;
    }) { inherit libffi; };
    libxkbcommon =
      (portable (pkgs.libxkbcommon.override {
        withWaylandTools = false;
      }) { inherit libxcb libx11; }).overrideAttrs
        (_: {
          outputs = [
            "out"
            "dev"
          ];
          nativeBuildInputs = with pkgs; [
            meson
            ninja
            pkg-config
            bison
            binutils
          ];
          buildInputs = [ libxcb ];
          mesonFlags = [
            "-Denable-x11=true"
            "-Denable-wayland=false"
            "-Denable-tools=false"
            "-Denable-docs=false"
            "-Denable-xkbregistry=false"
            "-Dxkb-config-root=/usr/share/X11/xkb"
            "-Dxkb-config-extra-path=/etc/xkb"
            "-Dx-locale-root=/usr/share/X11/locale"
          ];
        });
    # Host ALSA configuration/plugins remain authoritative. Do not add Nix
    # configuration symlinks that cannot exist on the receiving desktop.
    alsa =
      (portable (pkgs.alsa-lib.overrideAttrs (_: {
        postInstall = "";
      })) { }).overrideAttrs
        (old: {
          # Upstream checks only whether the header declares closefrom(), while
          # glibc introduced its linkable symbol in 2.34. Use its existing close()
          # loop on the supported 2.31 baseline even with newer Zig headers.
          env = old.env // {
            ac_cv_have_decl_closefrom = "no";
          };
          postConfigure = (old.postConfigure or "") + ''
            substituteInPlace include/config.h \
              --replace-fail "$out/share/alsa" /usr/share/alsa \
              --replace-fail "$out/lib/alsa-lib" /usr/lib/x86_64-linux-gnu/alsa-lib
          '';
        });
  };
  libraries = map lib.getLib (lib.attrValues runtime);
in
{
  inherit libraries;
  # Packaging reads exactly these approved output roots, not a whole Nix
  # closure containing current glibc or GPU drivers from build-time tools.
  runtimePaths = pkgs.writeText "maple-portable-runtime-roots" (
    lib.concatMapStringsSep "\n" toString libraries + "\n"
  );
}
