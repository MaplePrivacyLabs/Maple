{
  description = "Rust project";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      # Pin the first upstream revision compatible with Nixpkgs' current
      # fetchurl naming semantics while preserving the Rust 1.90.0 toolchain.
      url = "github:oxalica/rust-overlay/37f8f092415b444c3bed6eda6bcbee51cee22e5d";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Keep the Nitro helper compiler independent from the application's pinned
    # Rust toolchain. NSM API v0.5.2 requires a newer compiler than the backend.
    nitro-rust-overlay = {
      url = "github:oxalica/rust-overlay/b6916ba032e02122d6ed3064f40cabe937363d43";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    # Keep dev security tools current without moving the application or Nitro build pin.
    security-tools-nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    # Pin SecretSpec 0.20 / BWS independently of application and enclave dependencies.
    secrets-nixpkgs.url = "github:NixOS/nixpkgs/e5ead30d0824debba629dcf0720abeddee57b7d6";
    nitro-util = {
      url = "github:monzo/aws-nitro-util/7d755578b0b0b9850c0d7c4738a6c8daf3ff55c0";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, secrets-nixpkgs, security-tools-nixpkgs, flake-utils, rust-overlay, nitro-rust-overlay, nitro-util }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        # Nixpkgs 26.05 still packages the libtpms v0.10.2 tag, which predates
        # its one-line GCC 15 const-correctness fix. Keep the Linux dev shell
        # buildable without changing warnings or moving the package revision.
        libtpmsGcc15FixOverlay = final: prev: {
          libtpms = prev.libtpms.overrideAttrs (oldAttrs: {
            patches = (oldAttrs.patches or [ ]) ++ [
              (final.fetchpatch2 {
                url = "https://github.com/stefanberger/libtpms/commit/a20f8b6a22f1ae60d96ae7e554f5e13dd431471b.patch?full_index=1";
                hash = "sha256-gOm4LCFd7lKJDaLFfcQdtNXtU9QJLn3PMdoQXXm+myI=";
              })
            ];
          });
        };
        overlays = [
          rust-overlay.overlays.default
          libtpmsGcc15FixOverlay
        ];
        pkgs = import nixpkgs { inherit system overlays; };
        nitroRustPkgs = import nixpkgs {
          inherit system;
          overlays = [ nitro-rust-overlay.overlays.default ];
        };
        securityToolsPkgs = import security-tools-nixpkgs { inherit system; };
        secretsPkgs = import secrets-nixpkgs {
          inherit system;
          config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "bws";
        };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        nitro = nitro-util.lib.${system};
        kernelUpstream = import ./nix/kernel-upstream.nix;
        nitroHelperUpstreams = import ./nix/nitro-bins/upstreams.nix;
        nitroRustToolchainVersion = nitroHelperUpstreams.rust.version;
        nitroRustToolchain = nitroRustPkgs.rust-bin.stable."${nitroRustToolchainVersion}".minimal;

        # Development environment setup
        # Get rust-analyzer matching the channel in rust-toolchain.toml
        rustToolchain = builtins.fromTOML (builtins.readFile ./rust-toolchain.toml);
        rustChannel = rustToolchain.toolchain.channel;
        rustAnalyzer = pkgs.rust-bin.stable."${rustChannel}".rust-analyzer;
        appRustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };

        commonInputs = [
          rust
          rustAnalyzer
          pkgs.pkg-config
          pkgs.openssl
          pkgs.zlib
          pkgs.gcc
          pkgs.clang
          pkgs.jq
          pkgs.just
          pkgs.postgresql
          pkgs.diesel-cli
          pkgs.python3
          (pkgs.python3.withPackages (ps: with ps; [
            cryptography
          ]))
          pkgs.go
        ];
        linuxOnlyInputs = [
          pkgs.podman
          pkgs.conmon
          pkgs.slirp4netns
          pkgs.fuse-overlayfs
        ];
        darwinOnlyInputs = [
          pkgs.libiconv
          pkgs.apple-sdk
        ];
        securityToolInputs = [
          securityToolsPkgs.cargo-audit
          securityToolsPkgs.cargo-deny
          securityToolsPkgs.cargo-machete
        ];
        inputs = commonInputs
          ++ securityToolInputs
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux linuxOnlyInputs
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin darwinOnlyInputs;

        setupPostgresScript = pkgs.writeShellScript "setup-postgres" ''
          case "''${OPENSECRET_DEV_POSTGRES:-1}" in
            0|false|False|FALSE|no|No|NO|skip|Skip|SKIP)
              exit 0
              ;;
          esac

          export PGDATA="''${PGDATA:-$PWD/.pgdata}"
          export PGPORT="''${PGPORT:-5432}"
          export PGSOCKETS="''${PGSOCKETS:-$PGDATA/sockets}"

          # Skip if Postgres is already running
          if ${pkgs.postgresql}/bin/pg_isready -h localhost -p $PGPORT >/dev/null 2>&1; then
            exit 0
          fi

          # Initialize if needed
          if [ ! -f "$PGDATA/PG_VERSION" ]; then
            ${pkgs.postgresql}/bin/initdb -D "$PGDATA"
          fi

          # Ensure socket directory exists
          mkdir -p "$PGSOCKETS"

          # Start Postgres
          ${pkgs.postgresql}/bin/pg_ctl start -D "$PGDATA" -o "-h localhost -p $PGPORT -k $PGSOCKETS" -l "$PGDATA/logfile" -w

          # Wait for it to be ready
          until ${pkgs.postgresql}/bin/pg_isready -h localhost -p $PGPORT >/dev/null 2>&1; do sleep 0.5; done

          # Create user and database if they don't exist
          ${pkgs.postgresql}/bin/psql -h localhost -p $PGPORT -tc "SELECT 1 FROM pg_roles WHERE rolname='opensecret_user'" postgres 2>/dev/null | grep -q 1 || \
            ${pkgs.postgresql}/bin/psql -h localhost -p $PGPORT -c "CREATE USER \"opensecret_user\" WITH PASSWORD 'password';" postgres
          ${pkgs.postgresql}/bin/psql -h localhost -p $PGPORT -tc "SELECT 1 FROM pg_database WHERE datname='opensecret'" postgres 2>/dev/null | grep -q 1 || \
            ${pkgs.postgresql}/bin/psql -h localhost -p $PGPORT -c "CREATE DATABASE \"opensecret\" OWNER \"opensecret_user\";" postgres
        '';

        setupEnvScript = pkgs.writeShellScript "setup-env" ''
          case "''${OPENSECRET_DEV_ENV:-1}" in
            0|false|False|FALSE|no|No|NO|skip|Skip|SKIP)
              exit 0
              ;;
          esac

          if [ ! -f .env ]; then
            cp .env.sample .env

            replace_env() {
              local pattern="$1"
              local replacement="$2"
              local tmp
              tmp="$(mktemp)"
              sed "s|$pattern|$replacement|g" .env > "$tmp"
              mv "$tmp" .env
            }

            export PGPORT="''${PGPORT:-5432}"
            export OPENSECRET_DEV_DATABASE_URL="''${OPENSECRET_DEV_DATABASE_URL:-postgres://opensecret_user:password@localhost:$PGPORT/opensecret}"
            replace_env 'DATABASE_URL=postgres://localhost/opensecret' "DATABASE_URL=$OPENSECRET_DEV_DATABASE_URL"

            # Get a new ENCLAVE_SECRET_MOCK value using openssl
            export enclaveSecret=$(openssl rand -hex 32)
            replace_env 'ENCLAVE_SECRET_MOCK=' "ENCLAVE_SECRET_MOCK=$enclaveSecret"

            # Get a new JWT_SECRET value using openssl
            export jwtSecret=$(openssl rand -base64 32)
            replace_env 'JWT_SECRET=' "JWT_SECRET=$jwtSecret"
          fi
        '';

        # Function to create rootfs with specific APP_MODE
        mkRootfs = { appMode, opensecretPkg ? opensecret }: pkgs.buildEnv {
          name = "opensecret-rootfs-${appMode}";
          paths = [
            opensecretPkg
            (pkgs.writeScriptBin "entrypoint" ''
              #!${pkgs.bash}/bin/bash

              # Set up busybox commands and other tools
              export PATH="/bin:${pkgs.busybox}/bin:${pkgs.python3}/bin:${pkgs.jq}/bin:${pkgs.socat}/bin:${nitro-bins}/bin:$PATH"

              # Create symlinks for busybox commands
              mkdir -p /bin
              ln -sf ${pkgs.busybox}/bin/busybox /bin/date
              ln -sf ${pkgs.busybox}/bin/busybox /bin/ip
              ln -sf ${pkgs.python3}/bin/python3 /bin/python3
              ln -sf ${pkgs.jq}/bin/jq /bin/jq
              ln -sf ${pkgs.socat}/bin/socat /bin/socat
              ln -sf ${pkgs.curl}/bin/curl /bin/curl

              # Set up CA certificates
              mkdir -p /etc/ssl/certs
              ln -sf ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt /etc/ssl/certs/ca-bundle.crt
              export SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt
              export AWS_CA_BUNDLE=/etc/ssl/certs/ca-bundle.crt

              # Copy required libraries and tools
              mkdir -p /lib
              export LD_LIBRARY_PATH="/lib:$LD_LIBRARY_PATH"
              # NSM v0.5.2 uses SONAME libnsm.so.0. Preserve the complete
              # linker/runtime symlink chain in the enclave rootfs.
              cp -P ${nitro-bins}/lib/libnsm.so* /lib/
              install -m 755 ${nitro-bins}/lib/libgcc_s.so.1 /lib/

              install -m 755 ${nitro-bins}/bin/kmstool_enclave_cli /bin/

              # Copy required C libraries
              cp -P ${pkgs.glibc}/lib/ld-linux*.so* /lib/
              cp -P ${pkgs.glibc}/lib/libc.so* /lib/
              cp -P ${pkgs.glibc}/lib/libdl.so* /lib/
              cp -P ${pkgs.glibc}/lib/libpthread.so* /lib/
              cp -P ${pkgs.glibc}/lib/librt.so* /lib/
              cp -P ${pkgs.glibc}/lib/libm.so* /lib/

              # Set up Python environment
              export PYTHONPATH="$(find ${pkgs.python3}/lib -name site-packages):$PYTHONPATH"

              # Copy opensecret and the remaining Continuum sidecar.
              mkdir -p /app
              install -m 755 ${opensecretPkg}/bin/opensecret /app/
              install -m 755 ${continuum-proxy}/bin/continuum-proxy /app/

              ${builtins.readFile ./entrypoint.sh}
            '')
            (pkgs.writeTextFile {
              name = "app-mode";
              text = builtins.trace "Creating APP_MODE file with value: ${appMode}" appMode;
              destination = "/app/APP_MODE";
            })
            (pkgs.writeTextFile {
              name = "traffic_forwarder";
              text = builtins.readFile ./nitro-toolkit/traffic_forwarder.py;
              destination = "/app/traffic_forwarder.py";
            })
            (pkgs.writeTextFile {
              name = "vsock_helper";
              text = builtins.readFile ./nitro-toolkit/vsock_helper.py;
              destination = "/app/vsock_helper.py";
            })
            pkgs.bash
            pkgs.busybox
            pkgs.openssl
            pkgs.postgresql
            pkgs.socat
            pkgs.python3
            pkgs.jq
            pkgs.iproute2
            pkgs.coreutils
            pkgs.cacert
            pkgs.curl
            nitro-bins
            continuum-proxy
          ];
          pathsToLink = [ "/bin" "/lib" "/app" "/usr/bin" "/usr/sbin" "/sbin" ];
        };

        # Nixpkgs remains the build framework, but the enclave kernel's stable
        # patch level is pinned directly to kernel.org so security fixes do not
        # wait for a Nixpkgs refresh.
        expectedKernelUrl =
          "https://cdn.kernel.org/pub/linux/kernel/v${pkgs.lib.versions.major kernelUpstream.version}.x/linux-${kernelUpstream.version}.tar.xz";
        kernelSource = pkgs.fetchurl {
          inherit (kernelUpstream) url hash;
        };
        kernelStructuredExtraConfig = with pkgs.lib.kernel; {
          VIRTIO = yes;
          VIRTIO_MMIO = yes;
          VIRTIO_MENU = yes;
          VIRTIO_MMIO_CMDLINE_DEVICES = yes;
          NET = yes;
          VSOCKETS = yes;
          VIRTIO_VSOCKETS = yes;
          HW_RANDOM = yes;
          NSM = yes; # Enable the in-tree NSM driver for KMS operations.
          # Keep the unused AF_ALG AEAD socket adapter disabled. This does not
          # affect OpenSecret's userspace AEAD implementations.
          CRYPTO_USER_API_AEAD = no;
        };
        unverifiedCustomKernel = pkgs.linux_6_12.override {
          argsOverride = {
            inherit (kernelUpstream) version;
            modDirVersion = kernelUpstream.version;
            src = kernelSource;
          };
          structuredExtraConfig = kernelStructuredExtraConfig;
          # Fail if a required option is renamed or becomes unavailable.
          ignoreConfigErrors = false;
        };
        customKernel =
          assert pkgs.lib.assertMsg (
            kernelUpstream.branch == "6.12"
          ) "The enclave kernel must remain on the reviewed 6.12 LTS branch";
          assert pkgs.lib.assertMsg (
            pkgs.lib.versions.majorMinor kernelUpstream.version == kernelUpstream.branch
          ) "The enclave kernel version must match its declared LTS branch";
          assert pkgs.lib.assertMsg (
            kernelUpstream.url == expectedKernelUrl
          ) "The enclave kernel URL must be the version-matched kernel.org stable tarball";
          assert pkgs.lib.assertMsg (
            pkgs.lib.hasPrefix "sha256-" kernelUpstream.hash
          ) "The enclave kernel source must use an immutable SRI sha256 hash";
          assert pkgs.lib.assertMsg (
            unverifiedCustomKernel.version == kernelUpstream.version
          ) "The realized enclave kernel version differs from the direct upstream pin";
          assert pkgs.lib.assertMsg (
            unverifiedCustomKernel.modDirVersion == kernelUpstream.version
          ) "The realized enclave kernel module version differs from the direct upstream pin";
          assert pkgs.lib.assertMsg (
            kernelStructuredExtraConfig.NSM == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.HW_RANDOM == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VIRTIO == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VIRTIO_MMIO == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VIRTIO_MENU == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VIRTIO_MMIO_CMDLINE_DEVICES == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.NET == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VSOCKETS == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.VIRTIO_VSOCKETS == pkgs.lib.kernel.yes
            && kernelStructuredExtraConfig.CRYPTO_USER_API_AEAD == pkgs.lib.kernel.no
          ) "The enclave kernel's required NSM, hwrng, virtio, vsock, networking, or AEAD configuration changed";
          unverifiedCustomKernel;

        kernelSourcePin = pkgs.runCommand "opensecret-kernel-source-pin-${kernelUpstream.version}"
          {
            nativeBuildInputs = [
              pkgs.gnugrep
              pkgs.gnutar
              pkgs.xz
            ];
          }
          ''
            set -euo pipefail

            test -s ${kernelSource}
            tar -xOf ${kernelSource} \
              linux-${kernelUpstream.version}/drivers/misc/nsm.c > nsm.c

            # 6.12.101 must carry both reviewed NSM fixes: malformed userspace
            # pointers return before the mutex is touched, and file operations
            # retain module ownership.
            ioctl_source="$(sed -n \
              '/static long nsm_dev_ioctl/,/static int nsm_device_init_vq/p' \
              nsm.c)"
            printf '%s\n' "$ioctl_source" > nsm_dev_ioctl.c

            efault_line="$(grep -nF 'r = -EFAULT;' nsm_dev_ioctl.c | head -n1 | cut -d: -f1)"
            copy_line="$(grep -nF 'if (copy_from_user(&raw, argp, _IOC_SIZE(cmd)))' nsm_dev_ioctl.c | cut -d: -f1)"
            return_line="$(grep -nF 'return r;' nsm_dev_ioctl.c | head -n1 | cut -d: -f1)"
            mutex_line="$(grep -nF 'mutex_lock(&nsm->lock);' nsm_dev_ioctl.c | cut -d: -f1)"

            test -n "$efault_line"
            test -n "$copy_line"
            test -n "$return_line"
            test -n "$mutex_line"
            test "$efault_line" -lt "$copy_line"
            test "$copy_line" -lt "$return_line"
            test "$return_line" -lt "$mutex_line"

            fops_source="$(sed -n \
              '/static const struct file_operations nsm_dev_fops = {/,/};/p' \
              nsm.c)"
            printf '%s\n' "$fops_source" | grep -Fq '.owner = THIS_MODULE,'
            printf '%s\n' "$fops_source" | grep -Fq '.unlocked_ioctl = nsm_dev_ioctl,'

            {
              echo 'branch=${kernelUpstream.branch}'
              echo 'version=${kernelUpstream.version}'
              echo 'source=${kernelUpstream.url}'
              echo 'hash=${kernelUpstream.hash}'
              echo 'store_path=${kernelSource}'
            } > "$out"
          '';

        kernelSecurityInvariants = pkgs.runCommand
          "opensecret-kernel-security-invariants-${kernelUpstream.version}"
          { nativeBuildInputs = [ pkgs.gnugrep ]; }
          ''
            set -euo pipefail

            config=${customKernel.configfile}
            grep -Fqx 'CONFIG_VIRTIO=y' "$config"
            grep -Fqx 'CONFIG_VIRTIO_MMIO=y' "$config"
            grep -Fqx 'CONFIG_VIRTIO_MENU=y' "$config"
            grep -Fqx 'CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y' "$config"
            grep -Fqx 'CONFIG_NET=y' "$config"
            grep -Fqx 'CONFIG_VSOCKETS=y' "$config"
            grep -Fqx 'CONFIG_VIRTIO_VSOCKETS=y' "$config"
            grep -Fqx 'CONFIG_HW_RANDOM=y' "$config"
            grep -Fqx 'CONFIG_NSM=y' "$config"
            grep -Fqx '# CONFIG_CRYPTO_USER_API_AEAD is not set' "$config"

            {
              echo 'branch=${kernelUpstream.branch}'
              echo 'version=${kernelUpstream.version}'
              echo 'source=${kernelUpstream.url}'
              echo 'hash=${kernelUpstream.hash}'
              echo 'cmdline=${enclaveKernelCmdline}'
            } > "$out"
          '';

        entrypointEntropyPreflight = pkgs.runCommand
          "opensecret-entrypoint-entropy-preflight"
          {
            nativeBuildInputs = [
              pkgs.bash
              pkgs.coreutils
              pkgs.gnused
            ];
          }
          ''
            ENTRYPOINT_UNDER_TEST=${./entrypoint.sh} \
              bash ${./tests/entrypoint_entropy_preflight.sh}
            touch "$out"
          '';

        # aws-nitro-util already source-builds this init by default. Keep the
        # same source and build flags explicit while using the current
        # buildGoModule API, so a Nixpkgs refresh cannot revive the deprecated
        # top-level CGO_ENABLED argument.
        nitroInitSrc = builtins.path {
          path = "${nitro-util}/init";
          name = "init";
        };
        nitroInit = pkgs.buildGoModule {
          name = "eif-init";
          src = nitroInitSrc;
          vendorHash = null;
          env.CGO_ENABLED = "0";
          ldflags = [
            "-s"
            "-w"
          ];
        };

        # Preserve the currently deployed Nitro boot behavior explicitly. A
        # trust-policy experiment belongs in its own held draft PR.
        enclaveKernelCmdline =
          "reboot=k panic=30 pci=off nomodules console=ttyS0 random.trust_cpu=on root=/dev/ram0";

        # Function to create EIF with specific APP_MODE
        mkEif = { appMode, opensecretPkg ? opensecret, nameSuffix ? "" }: nitro.buildEif {
          name = "opensecret-eif-${appMode}${nameSuffix}";
          # The kernel image location varies by architecture
          kernel = if arch == "aarch64"
            then "${customKernel}/Image"  # ARM64 uses Image
            else "${customKernel}/bzImage"; # x86_64 uses bzImage
          # EIF metadata must describe the exact configuration used to build
          # this kernel, not the unrelated pre-built Nitro blob configuration.
          kernelConfig = customKernel.configfile;
          cmdline = enclaveKernelCmdline;
          # NSM driver is built into kernel 6.8+, so we don't need the old module
          # Setting to null to skip loading the incompatible old module
          nsmKo = null;
          copyToRoot = mkRootfs { inherit appMode opensecretPkg; };
          entrypoint = "/bin/entrypoint";
          init = "${nitroInit}/bin/init";
        };

        opensecret = appRustPlatform.buildRustPackage {
          pname = "opensecret";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                baseName = baseNameOf path;
                parts = pkgs.lib.splitString "/" path;
              in
                # Explicitly exclude .env files
                (baseName != ".env" && baseName != ".env.sample") &&
                (
                  (builtins.elem "src" parts) ||
                  (type == "regular" && (
                    baseName == "Cargo.toml" ||
                    baseName == "Cargo.lock" ||
                    baseName == "rust-toolchain.toml"
                  ))
                );
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "tinfoil-0.2.0" = "sha256-+J/9OtgO8kNba9UoWEmGuzpTxPKVV8VuSl05mNe8Kpw=";
              "tinfoil-ehbp-0.3.1" = "sha256-BSSob5uyIIOw/Zr5aZFTU0GOM5imxk7KoZ5g+I4+s70=";
            };
          };
          nativeBuildInputs = [
            pkgs.pkg-config
            rustAnalyzer
            pkgs.gcc
            pkgs.clang
          ];
          buildInputs = [
            pkgs.openssl
            pkgs.zlib
            pkgs.postgresql
          ];
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          LIBPQ_LIB_DIR = "${pkgs.postgresql.lib}/lib";
        };

        # Build the reviewed modern NSM and KMS helper closure from pinned
        # sources without moving the application's Rust toolchain.
        nitro-bins = import ./nix/nitro-bins {
          inherit pkgs;
          rustToolchain = nitroRustToolchain;
          rustToolchainVersion = nitroRustToolchainVersion;
        };


        # Copy continuum-proxy from local filesystem
        continuum-proxy = pkgs.runCommand "continuum-proxy" {} ''
          mkdir -p $out/bin
          cp ${./continuum-proxy} $out/bin/continuum-proxy
          chmod +x $out/bin/continuum-proxy
        '';

        arch = pkgs.stdenv.hostPlatform.uname.processor;
      in
      {
        packages = {
          default = opensecret;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nitro-init = nitroInit;
          inherit nitro-bins;
          eif-dev = mkEif { appMode = "dev"; };
          eif-prod = mkEif { appMode = "prod"; };
          eif-preview = mkEif { appMode = "preview"; };
        };

        checks = {
          entrypoint-entropy-preflight = entrypointEntropyPreflight;
          kernel-source-pin = kernelSourcePin;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          kernel-security-invariants = kernelSecurityInvariants;
          nitro-helper = nitro-bins;
        };

        devShell = pkgs.mkShell {
          packages = inputs ++ [ secretsPkgs.secretspec secretsPkgs.bws ];
          shellHook = ''
            export PGDATA="''${PGDATA:-$PWD/.pgdata}"
            export PGPORT="''${PGPORT:-5432}"
            export PGSOCKETS="''${PGSOCKETS:-$PGDATA/sockets}"
            export OPENSECRET_DEV_DATABASE_URL="''${OPENSECRET_DEV_DATABASE_URL:-postgres://opensecret_user:password@localhost:$PGPORT/opensecret}"

            export LIBCLANG_PATH=${pkgs.libclang.lib}/lib/
            export LD_LIBRARY_PATH=${pkgs.openssl}/lib:$LD_LIBRARY_PATH
            export CC_wasm32_unknown_unknown=${pkgs.llvmPackages.clang-unwrapped}/bin/clang
            export CFLAGS_wasm32_unknown_unknown="-I ${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.versions.major pkgs.llvmPackages.libclang.version}/include/"
            export PKG_CONFIG_PATH=${pkgs.openssl.dev}/lib/pkgconfig

            ${pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
              export CC=clang
              export CXX=clang++
            ''}

            ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              alias docker='podman'
              echo "Using 'podman' as an alias for 'docker'"
              echo "You can now use 'docker' commands, which will be executed by podman"

              case "''${OPENSECRET_DEV_CONTAINERS:-1}" in
                0|false|False|FALSE|no|No|NO|skip|Skip|SKIP)
                  echo "Skipping development container configuration"
                  ;;
                1|true|True|TRUE|yes|Yes|YES)
                  # Podman configuration
                  export CONTAINERS_CONF="$HOME/.config/containers/containers.conf"
                  export CONTAINERS_POLICY="$HOME/.config/containers/policy.json"
                  mkdir -p "$HOME/.config/containers"
                  echo '{"default":[{"type":"insecureAcceptAnything"}]}' > "$CONTAINERS_POLICY"

                  # Create a basic containers.conf if it doesn't exist
                  if [ ! -f "$CONTAINERS_CONF" ]; then
                    echo "[engine]
                  cgroup_manager = \"cgroupfs\"
                  events_logger = \"file\"
                  runtime = \"crun\"

                  [storage]
                  driver = \"vfs\"" > "$CONTAINERS_CONF"
                  fi

                  # Ensure correct permissions
                  chmod 600 "$CONTAINERS_POLICY" "$CONTAINERS_CONF"
                  ;;
                *)
                  echo "ERROR: OPENSECRET_DEV_CONTAINERS must be 0/1, false/true, no/yes, or skip" >&2
                  exit 1
                  ;;
              esac
            ''}

            ${setupPostgresScript}
            ${setupEnvScript}
          '';
        };
      }
    );
}
