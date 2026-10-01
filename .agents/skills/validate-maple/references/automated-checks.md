# Maple automated and platform checks

Read only the component/platform section relevant to the change. Commands
run from the monorepo root unless they explicitly change directory. Use the
selected [environment](../../../../docs/development-environments.md) for runtime
evidence; these fixed PR artifacts use hosted Dev profiles.

### Standalone hosted Auth

For changes confined to `apps/maple-auth`, use its own guide and checks:

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/auth-ci.sh
MAPLE_AUTH_ENVIRONMENT=pr nix develop --no-update-lock-file .#ci -c ./scripts/ci/auth-web.sh
```

Auth owns its package, registry SDK pin, frozen lockfile, tests, assets, and
`dist` build. Do not install Research dependencies or run its web/native
packaging merely to validate Auth. Shared publisher/workflow changes still
require the repository checks. Real provider callbacks, retained sessions,
manual/native opening, and live edge behavior require separate rehearsal;
a local artifact does not establish those results. Browser smoke must serve
Auth's built `dist` with its own preview command and record its origin.

### Focused frontend test

```bash
nix develop --no-update-lock-file .#ci -c bash -lc \
  'cd apps/maple-research/frontend && bun --no-env-file test ./src/path/to/changed.test.ts'
```

Use `--no-env-file` to disable Bun's automatic dotenv loading. Variables
already exported by the calling shell still apply.

### Complete frontend checks

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/frontend.sh
```

This script installs locked frontend dependencies and runs formatting, linting, typechecking, and Bun tests. It removes `apps/maple-research/frontend/node_modules` and ignores local `.env*` files while it runs. Commit or preserve relevant local work before invoking it. It does **not** build the application.

### Web production build

```bash
MAPLE_WEB_ENVIRONMENT=pr nix develop --no-update-lock-file .#ci -c ./scripts/ci/web.sh
```

Use `pr` for contributor validation. Record the compiled OpenSecret, billing, and feature-flag endpoint configuration when those endpoints affect the scenario. A successful web build proves bundling, not browser behavior.

To claim browser smoke for that artifact, serve the resulting `apps/maple-research/frontend/dist`
with the checked-in preview command, then open that preview rather than the
development server:

```bash
nix develop --no-update-lock-file .#ci -c bash -lc \
  'cd apps/maple-research/frontend && bun --no-env-file run preview'
```

Record the preview origin, server PID and checkout, compiled endpoints, browser
storage/profile, and account/data scope. Use a disposable account only when
sign-in or user data is required. Opening `just dev` is configured
development-runtime evidence, not smoke evidence for the built artifact.

### Rust checks

For CI parity run `scripts/ci/rust.sh`; strict `just rust-lint` below is optional.

```bash
nix develop --no-update-lock-file .#ci -c just rust-lint
nix develop --no-update-lock-file .#ci -c ./scripts/ci/rust.sh
```

The CI script provisions Linux ONNX Runtime when needed and runs
`cargo test --all-targets --locked`; Research CI and its hook do not run Clippy.
`just rust-lint` is a separate optional strict diagnostic, not current CI
parity. Report vendored/dependency warnings separately rather than changing
unrelated patches to satisfy it. Neither command launches Maple.

### Proxy checks and selected dependency graph

Run the proxy's CI-equivalent component checks through its pinned shell:

```bash
nix develop --no-update-lock-file ./proxy -c bash -lc '
  set -euo pipefail
  cd proxy
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked --all-features
  RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
  cargo machete
'
```

For proxy dependency-wiring or Rust SDK runtime changes, also prove Research
resolves one SDK from its selected source and the in-tree proxy:

```bash
nix develop --no-update-lock-file .#ci -c \
  ./scripts/ci/verify-local-rust-deps.sh
```

The container build uses the Maple root as context because it copies
`proxy/` and `sdk/rust/`:

```bash
docker build -f proxy/Dockerfile -t maple-proxy:validation .
```

A component check or image build is not runtime evidence.

### Repository configuration checks

```bash
nix flake check --no-update-lock-file
```

This validates pinned tool versions, GitHub Actions syntax, and release metadata. Run it for changes to `flake.nix`, `flake.lock`, workflows, CI scripts, or release configuration. It is not a substitute for product tests.

Do not cite the pre-commit hook as complete proof: it runs only the format, lint, type-check, and unit-test lanes for the components whose files are staged, and never the integration suites, cargo-deny, `nix flake check`, or packaging.

## Build the affected platform

Use these commands to mirror PR artifact builds. Then launch and smoke the result separately.

These scripts deliberately hide local `.env*` files and compile fixed PR
endpoint profiles. Their artifacts are PR packaging evidence; they do not prove
that Maple works with the OpenSecret backend configured in
`apps/maple-research/frontend/.env.local`.

### macOS desktop

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/desktop-pr.sh
```

### Linux desktop

```bash
MAPLE_TAURI_FAKE_UPDATER_SIGNING=1 nix develop --no-update-lock-file .#desktop-linux -c ./scripts/ci/desktop-pr.sh
```

### Windows desktop

Run on real Windows from Git Bash/MSYS:

```bash
./scripts/ci/desktop-windows-pr.sh
```

### Android

Run on x86_64 Linux:

```bash
MAPLE_ANDROID_FAKE_SIGNING=1 MAPLE_ANDROID_WEB_ENVIRONMENT=pr nix develop --no-update-lock-file .#android -c ./scripts/ci/android-release.sh
```

### iOS

Run on macOS with the supported Xcode toolchain:

```bash
nix develop --no-update-lock-file .#apple -c ./scripts/ci/ios-onnxruntime.sh
nix develop --no-update-lock-file .#apple -c ./scripts/ci/ios-pr.sh
```

Treat PR artifact workflows as compile/package evidence. They do not launch the built app. Treat artifact attestations as provenance evidence only when the attestation step actually succeeds.
