# iOS ONNX Runtime Local Development Guide

This guide explains how to build and test the iOS ONNX Runtime used by PDF OCR on the iOS Simulator or a physical device.

## Overview

Maple pins ONNX Runtime 1.23.2 and links it statically on iOS for PDF OCR. ONNX Runtime is built from source because:
1. Pre-built binaries from HuggingFace are missing Abseil symbols
2. We need both device (arm64) and simulator (arm64) builds
3. The simulator build requires a workaround for a libiconv linking bug

## Prerequisites

Use macOS with full Xcode and its matching iOS Simulator runtime installed.
Select the supported Xcode used by the checked-in iOS recipes/workflows. Enter
the repository's pinned Apple shell, which supplies Rust, CMake, and Python:

```bash
nix develop --no-update-lock-file .#apple
```

Commands below run from the monorepo root in that shell. This guide covers
Research's Tauri/ONNX runtime. Choose [the client environment/login path](../../../docs/development-environments.md)
separately; a simulator or debug app can still use hosted services.

## Quick Start

### 1. Build ONNX Runtime

```bash
just ios-build-onnxruntime
```

This builds and hash-verifies ONNX Runtime for ARM64 device and simulator.
A valid cached artifact is reused; a stale or differently built artifact is
rebuilt. Cold builds generate Cargo config, but the valid-cache path exits
after verification. Generate it explicitly in the next step when missing,
after moving the checkout, or after changing its public native build settings.

The output will be in `apps/maple-research/frontend/src-tauri/onnxruntime-ios/onnxruntime.xcframework/`.

### 2. Generate Cargo Config

After obtaining the verified artifact, generate or refresh the checkout paths
and public native variant/PCR settings used by Cargo:

```bash
just ios-setup-cargo-config
```

This creates `apps/maple-research/frontend/src-tauri/.cargo/config.toml` with the correct absolute paths for your machine.

### 3. Fix arm64-sim Xcode Issue (if needed)

If you see this error:
```
clang: error: version '-sim' in target triple 'arm64-apple-ios13.0-simulator-sim' is invalid
```

See [troubleshooting-ios-build.md](./troubleshooting-ios-build.md) for details.

Quick fix:
```bash
just ios-fix-arch
```

### 4. Run on Simulator

```bash
# Boot simulator first
xcrun simctl boot "iPhone 16 Pro"

# Run the app
just ios-dev-sim "iPhone 16 Pro"
```

### 5. Run on Physical Device

```bash
just ios-dev-device "Device Name"
```

Select the target explicitly. `just ios-dev` may pick a connected physical
device even wirelessly. Physical-device networking to a Mac backend is separate
from simulator loopback; preserve the SDK's supported attestation/URL contract
in the [environment guide](../../../docs/development-environments.md#local-baseline-authentication).

## Troubleshooting

### Vite Server Not Reachable

The iOS simulator needs to connect to your development server. Ensure `apps/maple-research/frontend/vite.config.ts` has:

```typescript
server: {
  host: "0.0.0.0",
  port: 5173,
  strictPort: true
}
```

### Missing Abseil Symbols

If you see linker errors like:
```
Undefined symbols for architecture arm64:
  "_AbslInternalSpinLockDelay_lts_20240722"
```

This means the ONNX Runtime library wasn't built through Maple's verified source pipeline. Run `just ios-build-onnxruntime`.

### Simulator Build Fails with libiconv Error

If the simulator build fails with:
```
ld: building for 'iOS-simulator', but linking in dylib built for 'iOS'
```

This is fixed by adding `CMAKE_FIND_ROOT_PATH_MODE_LIBRARY=NEVER` to the cmake flags. The `build-ios-onnxruntime-all.sh` script already includes this fix.

### Cargo Not Finding Library

1. Ensure `.cargo/config.toml` uses absolute paths
2. Clean checkout-local artifacts with `nix develop --no-update-lock-file -c just clean-local`, then
   rebuild. Do not use raw `cargo clean` from a local Nix shell because its
   intermediate build directory may be shared with other Maple workspaces.
3. Verify the library exists: `ls -la apps/maple-research/frontend/src-tauri/onnxruntime-ios/onnxruntime.xcframework/ios-arm64-simulator/`

## Architecture Notes

### Why Build from Source?

1. **Abseil symbols**: ONNX Runtime depends on Abseil (Google's C++ library). Pre-built binaries don't include these statically linked.

2. **Simulator support**: Pre-built libraries often only include device builds.

3. **Version compatibility**: Building from source ensures compatibility with our ort-sys Rust crate version.

### Build Artifacts

After building, you'll have:
```
apps/maple-research/frontend/src-tauri/
├── onnxruntime-build/          # Build directory (can be deleted after build)
│   └── onnxruntime/            # ONNX Runtime source
└── onnxruntime-ios/            # Output directory
    └── onnxruntime.xcframework/
        ├── Headers/
        ├── Info.plist
        ├── ios-arm64/
        │   └── libonnxruntime.a     # Device library (~69MB)
        └── ios-arm64-simulator/
            └── libonnxruntime.a     # Simulator library (~69MB)
```

### .cargo/config.toml

The cargo config tells the Rust `ort-sys` crate where to find the ONNX Runtime library. The keys are:
- `[target.aarch64-apple-ios.onnxruntime]` - Device builds
- `[target.aarch64-apple-ios-sim.onnxruntime]` - Simulator builds (ARM64 Mac)

The current generator and artifact pipeline emit only those ARM64 targets;
they do not provide an Intel simulator library/configuration.

### CI/CD

The root iOS workflows build/hash-verify this artifact through the same recipe.
Cache keys include Xcode/build identity, pinned ONNX version, and relevant
scripts/toolchain inputs. Change the owning pins and inspect the workflow
contract rather than assuming a single environment variable controls reuse.

## Cleaning Up

To free disk space after testing:

```bash
# Remove build directory (keeps the built xcframework)
rm -rf apps/maple-research/frontend/src-tauri/onnxruntime-build

# Remove everything (requires rebuilding)
rm -rf apps/maple-research/frontend/src-tauri/onnxruntime-build apps/maple-research/frontend/src-tauri/onnxruntime-ios
```

## Related Documentation

- [troubleshooting-ios-build.md](./troubleshooting-ios-build.md) - arm64-sim architecture fix
- [pdf-ocr.md](./pdf-ocr.md) - PDF extraction and OCR architecture
