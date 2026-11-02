#!/bin/bash
# Build ONNX Runtime from source for iOS (device + simulator)
# This creates a static library with all dependencies (including Abseil) statically linked
#
# Prerequisites:
# - macOS with Xcode installed
# - CMake 3.26+
# - Python 3.8+
# - Git
#
# Usage: ./build-ios-onnxruntime-all.sh [version]
# Example: ./build-ios-onnxruntime-all.sh 1.23.2

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/onnxruntime-pins.sh"

TAURI_DIR="$(dirname "$SCRIPT_DIR")"
ORT_VERSION="${1:-1.23.2}"
ORT_COMMIT="${ORT_COMMIT:-$(onnxruntime_ios_commit_for_version "${ORT_VERSION}")}"
BUILD_DIR="${TAURI_DIR}/onnxruntime-build"
OUTPUT_DIR="${TAURI_DIR}/onnxruntime-ios"
XCFRAMEWORK_DIR="${OUTPUT_DIR}/onnxruntime.xcframework"

IOS_DEPLOYMENT_TARGET="16.0"

echo "========================================"
echo "Building ONNX Runtime ${ORT_VERSION} for iOS"
echo "(Device + Simulator)"
echo "========================================"
echo "Source commit: ${ORT_COMMIT}"
echo "Build directory: ${BUILD_DIR}"
echo "Output directory: ${OUTPUT_DIR}"
echo ""

# Check prerequisites
command -v cmake >/dev/null 2>&1 || { echo "Error: cmake is required"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "Error: python3 is required"; exit 1; }
command -v git >/dev/null 2>&1 || { echo "Error: git is required"; exit 1; }
command -v xcodebuild >/dev/null 2>&1 || { echo "Error: Xcode is required"; exit 1; }

# Check if output already exists
if [ -d "$XCFRAMEWORK_DIR" ] && [ -f "${XCFRAMEWORK_DIR}/ios-arm64/libonnxruntime.a" ] && [ -f "${XCFRAMEWORK_DIR}/ios-arm64-simulator/libonnxruntime.a" ]; then
    echo "ONNX Runtime xcframework already exists with both device and simulator libraries"
    echo "To rebuild, remove: rm -rf $OUTPUT_DIR"
    exit 0
fi

mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

# Clone and check out ONNX Runtime at the pinned source commit.
checkout_onnxruntime_source() {
    if [ ! -d "onnxruntime/.git" ]; then
        rm -rf onnxruntime
        git init onnxruntime
    fi

    (
        cd onnxruntime
        if ! git remote get-url origin >/dev/null 2>&1; then
            git remote add origin https://github.com/microsoft/onnxruntime.git
        fi
        git fetch --depth 1 origin "${ORT_COMMIT}"
        git checkout --force --detach FETCH_HEAD
        git submodule update --init --recursive --force

        local actual_commit
        actual_commit="$(git rev-parse HEAD)"
        if [ "${actual_commit}" != "${ORT_COMMIT}" ]; then
            echo "Expected ONNX Runtime commit ${ORT_COMMIT}, got ${actual_commit}" >&2
            return 1
        fi
    )
}

clone_with_retry() {
    local max_attempts=3
    local attempt=1
    while [ $attempt -le $max_attempts ]; do
        echo "Attempt $attempt of $max_attempts..."
        if checkout_onnxruntime_source; then
            return 0
        fi
        sleep 10
        attempt=$((attempt + 1))
    done
    return 1
}

echo "Checking out ONNX Runtime repository..."
clone_with_retry

cd onnxruntime

# ONNX Runtime embeds the source revision in its public build-info string. Use
# the full commit so the provenance is unambiguous. Older versions also embed
# CMAKE_CXX_FLAGS; canonicalize that optional field when it is present.
patch_reproducible_build_info() {
    python3 - "$PWD/cmake/CMakeLists.txt" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
old = 'string(APPEND ORT_BUILD_INFO ", cmake cxx flags: ${CMAKE_CXX_FLAGS}")'
new = '''set(ORT_BUILD_INFO_CXX_FLAGS "${CMAKE_CXX_FLAGS}")
if(DEFINED ORT_REPRODUCIBLE_BUILD_ROOT AND DEFINED ORT_REPRODUCIBLE_BUILD_ROOT_CANONICAL AND DEFINED ORT_REPRODUCIBLE_SOURCE_ROOT_CANONICAL)
  string(REPLACE "${ORT_REPRODUCIBLE_BUILD_ROOT}/onnxruntime" "${ORT_REPRODUCIBLE_SOURCE_ROOT_CANONICAL}" ORT_BUILD_INFO_CXX_FLAGS "${ORT_BUILD_INFO_CXX_FLAGS}")
  string(REPLACE "${ORT_REPRODUCIBLE_BUILD_ROOT}" "${ORT_REPRODUCIBLE_BUILD_ROOT_CANONICAL}" ORT_BUILD_INFO_CXX_FLAGS "${ORT_BUILD_INFO_CXX_FLAGS}")
endif()
string(APPEND ORT_BUILD_INFO ", cmake cxx flags: ${ORT_BUILD_INFO_CXX_FLAGS}")'''
old_commit = 'execute_process(COMMAND ${GIT_EXECUTABLE} log -1 --format=%h'
new_commit = 'execute_process(COMMAND ${GIT_EXECUTABLE} log -1 --format=%H'

changed = False
if new_commit not in text:
    if old_commit not in text:
        sys.exit("Could not normalize the ONNX Runtime build-info commit hash.")
    text = text.replace(old_commit, new_commit, 1)
    changed = True

if old in text and new not in text:
    text = text.replace(old, new, 1)
    changed = True

if changed:
    path.write_text(text)
PY
}

patch_reproducible_build_info

# ONNX Runtime object files embed absolute source paths via __FILE__ in several
# logging/status paths. Remap repo-specific build roots to stable prefixes before
# hashing the produced static libraries.
SOURCE_PREFIX_MAP_FLAGS=(
    "-ffile-prefix-map=${BUILD_DIR}/onnxruntime=/maple/third_party/onnxruntime"
    "-ffile-prefix-map=${BUILD_DIR}=/maple/build/onnxruntime"
    "-fdebug-prefix-map=${BUILD_DIR}/onnxruntime=/maple/third_party/onnxruntime"
    "-fdebug-prefix-map=${BUILD_DIR}=/maple/build/onnxruntime"
    "-fmacro-prefix-map=${BUILD_DIR}/onnxruntime=/maple/third_party/onnxruntime"
    "-fmacro-prefix-map=${BUILD_DIR}=/maple/build/onnxruntime"
)
SOURCE_PREFIX_MAP_CFLAGS="${SOURCE_PREFIX_MAP_FLAGS[*]}"

# Common cmake defines. build.sh --skip_tests prevents test execution, but the
# default CMake option still configures unit-test targets and looks for XCTest.
CMAKE_EXTRA_DEFINES=(
    "CMAKE_POLICY_VERSION_MINIMUM=3.5"
    "onnxruntime_BUILD_UNIT_TESTS=OFF"
    "onnxruntime_BUILD_BENCHMARKS=OFF"
    "ORT_REPRODUCIBLE_BUILD_ROOT=${BUILD_DIR}"
    "ORT_REPRODUCIBLE_BUILD_ROOT_CANONICAL=/maple/build/onnxruntime"
    "ORT_REPRODUCIBLE_SOURCE_ROOT_CANONICAL=/maple/third_party/onnxruntime"
    "CMAKE_ASM_FLAGS=${SOURCE_PREFIX_MAP_CFLAGS}"
    "CMAKE_C_FLAGS=${SOURCE_PREFIX_MAP_CFLAGS}"
    "CMAKE_CXX_FLAGS=${SOURCE_PREFIX_MAP_CFLAGS}"
    "CMAKE_OBJC_FLAGS=${SOURCE_PREFIX_MAP_CFLAGS}"
    "CMAKE_OBJCXX_FLAGS=${SOURCE_PREFIX_MAP_CFLAGS}"
)

# Function to build and combine libraries
build_and_combine() {
    local SYSROOT=$1
    local OUTPUT_SUFFIX=$2
    shift 2
    local EXTRA_CMAKE_DEFINES=("$@")
    
    echo ""
    echo "========================================"
    echo "Building for ${SYSROOT} (arm64)..."
    echo "========================================"
    
    # Clean previous build for this target
    rm -rf "build/iOS/Release"
    
    ./build.sh \
        --build_dir build/iOS \
        --config Release \
        --use_xcode \
        --ios \
        --apple_sysroot "${SYSROOT}" \
        --osx_arch arm64 \
        --apple_deploy_target "${IOS_DEPLOYMENT_TARGET}" \
        --parallel \
        --skip_tests \
        --compile_no_warning_as_error \
        --cmake_extra_defines "${CMAKE_EXTRA_DEFINES[@]}" "${EXTRA_CMAKE_DEFINES[@]}"
    
    # Find and combine libraries
    local BUILD_OUTPUT_DIR="build/iOS/Release/Release-${SYSROOT}"
    local COMBINED_LIB="${BUILD_OUTPUT_DIR}/libonnxruntime_combined.a"
    
    echo "Combining static libraries..."
    local LIBS=$(find build/iOS/Release -name "*.a" -path "*Release-${SYSROOT}*" -type f | grep -v "gtest\|gmock" | sort -u)
    
    if [ -z "$LIBS" ]; then
        echo "Error: No libraries found for ${SYSROOT}"
        return 1
    fi
    
    libtool -static -D -o "$COMBINED_LIB" $LIBS 2>&1 | grep -v "warning duplicate member" || true
    
    if [ ! -f "$COMBINED_LIB" ]; then
        echo "Error: Failed to create combined library"
        return 1
    fi

    local CANONICAL_LIB="${COMBINED_LIB}.canonical"
    python3 "${SCRIPT_DIR}/canonicalize-static-archive.py" "$COMBINED_LIB" "$CANONICAL_LIB"
    mv "$CANONICAL_LIB" "$COMBINED_LIB"
    ranlib -D "$COMBINED_LIB"
    
    echo "Created: $COMBINED_LIB ($(du -h "$COMBINED_LIB" | cut -f1))"
    
    # Copy to output
    mkdir -p "${XCFRAMEWORK_DIR}/${OUTPUT_SUFFIX}"
    cp "$COMBINED_LIB" "${XCFRAMEWORK_DIR}/${OUTPUT_SUFFIX}/libonnxruntime.a"
}

# Create output directories
mkdir -p "${OUTPUT_DIR}"
mkdir -p "${XCFRAMEWORK_DIR}/Headers"

# Build for device
build_and_combine "iphoneos" "ios-arm64"

# Build for simulator
build_and_combine "iphonesimulator" "ios-arm64-simulator"

# Copy headers
HEADER_FILE=$(find build -name "onnxruntime_c_api.h" -type f | head -n 1 || true)
if [ -n "$HEADER_FILE" ]; then
    HEADER_DIR=$(dirname "$HEADER_FILE")
    cp "${HEADER_DIR}"/*.h "${XCFRAMEWORK_DIR}/Headers/" 2>/dev/null || true
fi
if [ -d "include/onnxruntime/core/session" ]; then
    cp include/onnxruntime/core/session/*.h "${XCFRAMEWORK_DIR}/Headers/" 2>/dev/null || true
fi

if [ ! -f "${XCFRAMEWORK_DIR}/Headers/onnxruntime_c_api.h" ]; then
    echo "Error: failed to copy onnxruntime_c_api.h into ${XCFRAMEWORK_DIR}/Headers"
    exit 1
fi

# Create Info.plist
cat > "${XCFRAMEWORK_DIR}/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AvailableLibraries</key>
    <array>
        <dict>
            <key>HeadersPath</key>
            <string>Headers</string>
            <key>LibraryIdentifier</key>
            <string>ios-arm64</string>
            <key>LibraryPath</key>
            <string>libonnxruntime.a</string>
            <key>SupportedArchitectures</key>
            <array>
                <string>arm64</string>
            </array>
            <key>SupportedPlatform</key>
            <string>ios</string>
        </dict>
        <dict>
            <key>HeadersPath</key>
            <string>Headers</string>
            <key>LibraryIdentifier</key>
            <string>ios-arm64-simulator</string>
            <key>LibraryPath</key>
            <string>libonnxruntime.a</string>
            <key>SupportedArchitectures</key>
            <array>
                <string>arm64</string>
            </array>
            <key>SupportedPlatform</key>
            <string>ios</string>
            <key>SupportedPlatformVariant</key>
            <string>simulator</string>
        </dict>
    </array>
    <key>CFBundlePackageType</key>
    <string>XFWK</string>
    <key>XCFrameworkFormatVersion</key>
    <string>1.0</string>
</dict>
</plist>
PLIST

echo ""
echo "========================================"
echo "Build complete!"
echo "========================================"
echo ""
echo "xcframework: ${XCFRAMEWORK_DIR}"
echo ""
echo "Libraries:"
ls -lh "${XCFRAMEWORK_DIR}/ios-arm64/libonnxruntime.a"
ls -lh "${XCFRAMEWORK_DIR}/ios-arm64-simulator/libonnxruntime.a"
# Generate cargo config
echo ""
echo "Generating .cargo/config.toml..."
"${SCRIPT_DIR}/setup-ios-cargo-config.sh"

echo ""
echo "Next steps:"
echo "1. Fix arm64-sim issue if needed (see docs/troubleshooting-ios-build.md)"
echo "2. Run: just ios-dev-sim 'iPhone 16 Pro'"
