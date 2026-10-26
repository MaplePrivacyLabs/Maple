# Load environment variables from apps/maple-research/frontend/.env.local
set dotenv-path := "apps/maple-research/frontend/.env.local"
set dotenv-required := false

# List available commands
default:
    @just --list

# Install frontend dependencies
install:
    ./scripts/prepare-frontend-deps.sh

# Start the frontend development server
dev:
    cd apps/maple-research/frontend && bun run dev

build:
    cd apps/maple-research/frontend && bun run build

format:
    cd apps/maple-research/frontend && bun run format

lint:
    cd apps/maple-research/frontend && bun run lint

# Run Tauri iOS development build (default simulator) without the Rust file watcher
ios-dev: _verify-rust-lock
    cd apps/maple-research/frontend && bun run tauri ios dev --no-watch

# Run Tauri iOS development build on specific simulator (e.g., "iPhone 16 Pro iOS 26")
ios-dev-sim simulator: _verify-rust-lock
    cd apps/maple-research/frontend && bun run tauri ios dev --no-watch '{{simulator}}'

# Run Tauri iOS development build on physical device (e.g., "Your iPhone")
ios-dev-device device: _verify-rust-lock
    cd apps/maple-research/frontend && bun run tauri ios dev --no-watch --device '{{device}}'

# Build and verify ONNX Runtime for iOS (device + simulator) - used by PDF OCR
ios-build-onnxruntime:
    ./scripts/ci/ios-onnxruntime.sh

# Setup cargo config for iOS ONNX Runtime (run after building ONNX Runtime)
ios-setup-cargo-config:
    cd apps/maple-research/frontend/src-tauri && ./scripts/setup-ios-cargo-config.sh

# Fix arm64-sim Xcode architecture issue (run if you get arm64-sim errors)
ios-fix-arch:
    #!/usr/bin/env bash
    set -euo pipefail
    cd apps/maple-research/frontend/src-tauri/gen/apple
    echo "Fixing arm64-sim architecture issue..."
    perl -i -0pe 's/ARCHS = \(\s*arm64,\s*"arm64-sim",\s*\);/ARCHS = arm64;/g' maple.xcodeproj/project.pbxproj
    sed -i '' 's/VALID_ARCHS = "arm64  arm64-sim";/VALID_ARCHS = arm64;/g' maple.xcodeproj/project.pbxproj
    sed -i '' 's/"EXCLUDED_ARCHS\[sdk=iphoneos\*\]" = "arm64-sim x86_64";/"EXCLUDED_ARCHS[sdk=iphoneos*]" = x86_64;/g' maple.xcodeproj/project.pbxproj
    sed -i '' 's/"EXCLUDED_ARCHS\[sdk=iphonesimulator\*\]" = arm64;/"EXCLUDED_ARCHS[sdk=iphonesimulator*]" = "";/g' maple.xcodeproj/project.pbxproj
    echo "Fixed! Verify with: grep -E 'ARCHS =|VALID_ARCHS|EXCLUDED_ARCHS' maple.xcodeproj/project.pbxproj"

# Build Tauri Android release
android-build: _verify-rust-lock
    cd apps/maple-research/frontend/src-tauri && ./scripts/provide-android-onnxruntime.sh
    cd apps/maple-research/frontend && bun run tauri android build

# Build Tauri desktop release
desktop-build: _verify-rust-lock
    cd apps/maple-research/frontend && src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri build

# Run Tauri desktop development build without the Rust file watcher, using workspace-local config when available
desktop-dev: _verify-rust-lock
    cd apps/maple-research/frontend && if [ -f ../../../.local/tauri-workspace.json ]; then src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri dev --no-watch --config ../../../.local/tauri-workspace.json; else src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri dev --no-watch; fi

# Build Tauri desktop debug
desktop-build-debug: _verify-rust-lock
    cd apps/maple-research/frontend && src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri build --debug

# Build an unsigned debug desktop package with the required local Tauri overlay
desktop-build-debug-overlay: _verify-rust-lock
    #!/usr/bin/env bash
    set -euo pipefail
    test -f .local/tauri-workspace.json || { echo "missing .local/tauri-workspace.json" >&2; exit 1; }
    cd apps/maple-research/frontend
    src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri build --debug --no-sign --config ../../../.local/tauri-workspace.json --config '{"bundle":{"createUpdaterArtifacts":false}}'

# Build Tauri desktop release (with CC unset for compatibility)
desktop-build-no-cc: _verify-rust-lock
    cd apps/maple-research/frontend && unset CC && src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri build

# Build Tauri desktop debug (with CC unset for compatibility)
desktop-build-debug-no-cc: _verify-rust-lock
    cd apps/maple-research/frontend && unset CC && src-tauri/scripts/run-with-desktop-onnxruntime.sh bun tauri build --debug

# Format Rust code
rust-fmt:
    cd apps/maple-research/frontend/src-tauri && cargo fmt

# Fail before Tauri can build if Cargo.toml and Cargo.lock disagree.
_verify-rust-lock:
    cd apps/maple-research/frontend/src-tauri && cargo metadata --locked --format-version 1 >/dev/null

# Check Rust code compiles
rust-check:
    cd apps/maple-research/frontend/src-tauri && cargo check --locked

# Run Clippy lints on Rust code
rust-clippy:
    cd apps/maple-research/frontend/src-tauri && cargo clippy --locked

# Run all Rust checks (fmt check + clippy)
rust-lint:
    cd apps/maple-research/frontend/src-tauri && cargo fmt --check && cargo clippy --locked -- -D warnings

# Remove only this checkout's final Cargo artifacts. Raw `cargo clean` also
# removes CARGO_BUILD_BUILD_DIR, which may be shared by multiple workspaces.
clean-local:
    cd apps/maple-research/frontend/src-tauri && CARGO_BUILD_BUILD_DIR=target cargo clean

# Update version across all required files
update-version version:
    #!/usr/bin/env bash
    set -euo pipefail
    
    echo "Updating version to {{version}}..."
    
    # Parse version into components
    IFS='.' read -r major minor patch <<< "{{version}}"
    
    # Android versionCode is not user-visible. It only has to increase for
    # every uploaded Play Store build and must stay <= 2100000000.
    current_android_version_code=$(jq -r '.bundle.android.versionCode' apps/maple-research/frontend/src-tauri/tauri.conf.json)
    android_version_code=$((current_android_version_code + 1))
    if [ "$android_version_code" -gt 2100000000 ]; then
        echo "Error: Android versionCode $android_version_code exceeds Google Play maximum 2100000000."
        exit 1
    fi
    echo "Calculated Android versionCode: $android_version_code (previous + 1)"
    
    # Update package.json
    python3 scripts/update-version.py "{{version}}" "$android_version_code"
    
    # Run cargo check to update Cargo.lock
    echo "Running cargo check to update Cargo.lock..."
    cd apps/maple-research/frontend/src-tauri && cargo check
    
    echo "Version updated to {{version}} with Android versionCode $android_version_code in all files!"

# Get current version from package.json
get-version:
    @jq -r '.version' apps/maple-research/frontend/package.json

# Bump version by patch (0.0.1)
bump-patch:
    #!/usr/bin/env bash
    set -euo pipefail
    
    current=$(just get-version)
    IFS='.' read -r major minor patch <<< "$current"
    new_version="$major.$minor.$((patch + 1))"
    
    just update-version "$new_version"

# Bump version by minor (0.1.0)
bump-minor:
    #!/usr/bin/env bash
    set -euo pipefail
    
    current=$(just get-version)
    IFS='.' read -r major minor patch <<< "$current"
    new_version="$major.$((minor + 1)).0"
    
    just update-version "$new_version"

# Bump version by major (1.0.0)
bump-major:
    #!/usr/bin/env bash
    set -euo pipefail
    
    current=$(just get-version)
    IFS='.' read -r major minor patch <<< "$current"
    new_version="$((major + 1)).0.0"
    
    just update-version "$new_version"

# Increment Android versionCode for another Play Store upload with the same visible version
update-android-counter:
    #!/usr/bin/env bash
    set -euo pipefail
    
    # Get current versionCode from tauri.conf.json
    current_code=$(jq -r '.bundle.android.versionCode' apps/maple-research/frontend/src-tauri/tauri.conf.json)
    
    # Increment by one. This value is internal to Android/Play Store and is not user-visible.
    new_code=$((current_code + 1))
    
    # Ensure versionCode stays within Google Play's maximum.
    if [ "$new_code" -gt 2100000000 ]; then
        echo "Error: Android versionCode $new_code exceeds Google Play maximum 2100000000."
        exit 1
    fi
    
    echo "Updating Android versionCode: $current_code -> $new_code"
    
    # Update tauri.conf.json Android versionCode
    sed -i "s/\"versionCode\": $current_code/\"versionCode\": $new_code/" apps/maple-research/frontend/src-tauri/tauri.conf.json
    
    echo "Android versionCode updated to $new_code"

# Create a new release (updates version and creates git tag)
release version:
    just update-version {{version}}
    git add -A
    git commit -m "chore: bump version to {{version}}"
    git tag -a "v{{version}}" -m "Release v{{version}}"
    echo "Release v{{version}} created! Don't forget to push tags: git push && git push --tags"

# Run one ci.yml lane exactly as CI does (lane ids are in scripts/ci/lanes.py).
lane NAME:
    python3 scripts/ci/plan.py run {{NAME}}

# GPUI Agent uses its own pinned toolchain and component working directory.
agent-check:
    cd apps/maple-agent && nix develop --no-update-lock-file -c just ci

agent-build:
    cd apps/maple-agent && nix develop --no-update-lock-file -c just build

# For managed workspaces, use their bin/maple-agent launcher to load isolated state.
agent-dev *ARGS:
    cd apps/maple-agent && nix develop --no-update-lock-file -c just run {{ARGS}}

# Backend checks use its separate pinned toolchain, with stateful shell hooks off.
opensecret-check:
    cd services/opensecret && OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file -c bash -c 'set -euo pipefail; cargo fmt --check; cargo clippy --locked --all-targets --all-features -- -D warnings; cargo test --locked --all-features'

# Offline validation only; does not build an EIF, sign, copy, or publish files.
opensecret-pcr-check:
    cd services/opensecret && OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file -c python3 scripts/pcr_compatibility.py check .
