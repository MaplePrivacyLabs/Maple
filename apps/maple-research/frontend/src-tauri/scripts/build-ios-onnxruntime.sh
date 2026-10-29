#!/bin/bash
# Build ONNX Runtime from source for iOS
# This creates a static library with all dependencies (including Abseil) statically linked
#
# Prerequisites:
# - macOS with Xcode installed
# - CMake 3.26+
# - Python 3.8+
# - Git
#
# Usage: ./build-ios-onnxruntime.sh [version]
# Example: ./build-ios-onnxruntime.sh 1.23.2

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/onnxruntime-pins.sh"

TAURI_DIR="$(dirname "$SCRIPT_DIR")"
ORT_VERSION="${1:-1.23.2}"
ORT_COMMIT="${ORT_COMMIT:-$(onnxruntime_ios_commit_for_version "${ORT_VERSION}")}"
BUILD_DIR="${TAURI_DIR}/onnxruntime-build"
OUTPUT_DIR="${TAURI_DIR}/onnxruntime-ios"
XCFRAMEWORK_DIR="${OUTPUT_DIR}/onnxruntime.xcframework"

# Minimum iOS version to support
IOS_DEPLOYMENT_TARGET="16.0"

echo "========================================"
echo "Building ONNX Runtime ${ORT_VERSION} for iOS"
echo "========================================"
echo "Source commit: ${ORT_COMMIT}"
echo "Build directory: ${BUILD_DIR}"
echo "Output directory: ${OUTPUT_DIR}"
echo "iOS deployment target: ${IOS_DEPLOYMENT_TARGET}"
echo ""

# Check prerequisites
command -v cmake >/dev/null 2>&1 || { echo "Error: cmake is required but not installed."; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "Error: python3 is required but not installed."; exit 1; }
command -v git >/dev/null 2>&1 || { echo "Error: git is required but not installed."; exit 1; }
command -v xcodebuild >/dev/null 2>&1 || { echo "Error: Xcode is required but not installed."; exit 1; }

# Check if output already exists
if [ -d "$XCFRAMEWORK_DIR" ]; then
    echo "ONNX Runtime xcframework already exists at $XCFRAMEWORK_DIR"
    echo "To rebuild, remove the directory first: rm -rf $OUTPUT_DIR"
    exit 0
fi

# Create build directory
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
        git checkout --detach FETCH_HEAD
        git submodule update --init --recursive

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
        echo "Clone failed, waiting 10 seconds before retry..."
        sleep 10
        attempt=$((attempt + 1))
    done
    echo "Failed to clone after $max_attempts attempts"
    return 1
}

echo "Checking out ONNX Runtime repository..."
clone_with_retry

cd onnxruntime

# Common cmake extra defines to work around compatibility issues
# CMAKE_POLICY_VERSION_MINIMUM=3.5 fixes nsync compatibility with newer CMake
CMAKE_EXTRA_DEFINES="CMAKE_POLICY_VERSION_MINIMUM=3.5"

# Build for iOS device (arm64)
echo ""
echo "========================================"
echo "Building for iOS device (arm64)..."
echo "========================================"

./build.sh \
    --config Release \
    --use_xcode \
    --ios \
    --apple_sysroot iphoneos \
    --osx_arch arm64 \
    --apple_deploy_target "${IOS_DEPLOYMENT_TARGET}" \
    --parallel \
    --skip_tests \
    --compile_no_warning_as_error \
    --cmake_extra_defines "${CMAKE_EXTRA_DEFINES}"

# ONNX Runtime builds multiple static libraries, we need to combine them
# The libraries are in build/iOS/Release/Release-iphoneos/
IOS_ARM64_BUILD_DIR="build/iOS/Release/Release-iphoneos"
IOS_ARM64_COMBINED_LIB="${IOS_ARM64_BUILD_DIR}/libonnxruntime_combined.a"

echo ""
echo "Combining iOS arm64 static libraries..."

# Find all ONNX Runtime static libraries and combine them
# We need: onnxruntime_*, onnx*, protobuf-lite, re2, cpuinfo, abseil libs, etc.
IOS_ARM64_LIBS=$(find build/iOS/Release -name "*.a" -path "*Release-iphoneos*" -type f | grep -v "gtest\|gmock" | sort -u)

if [ -z "$IOS_ARM64_LIBS" ]; then
    echo "Error: Could not find iOS arm64 static libraries"
    exit 1
fi

echo "Found libraries to combine:"
echo "$IOS_ARM64_LIBS" | head -20
echo "..."

# Use libtool to combine all static libraries into one
libtool -static -o "$IOS_ARM64_COMBINED_LIB" $IOS_ARM64_LIBS

if [ ! -f "$IOS_ARM64_COMBINED_LIB" ]; then
    echo "Error: Failed to create combined library"
    exit 1
fi

IOS_ARM64_LIB="$IOS_ARM64_COMBINED_LIB"
echo "Created combined library: $IOS_ARM64_LIB"
ls -lh "$IOS_ARM64_LIB"

# SKIP SIMULATOR BUILD for now
# The simulator build has a bug where it tries to link against the wrong iconv library:
# "ld: building for 'iOS-simulator', but linking in dylib built for 'iOS'"
# For TestFlight/App Store deployment, we only need the device build anyway.
# Local development can use the desktop version or a physical device.
echo ""
echo "Skipping iOS simulator build (known ONNX Runtime CMake bug with libiconv)"
echo "Device build is sufficient for TestFlight deployment"
IOS_SIM_ARM64_LIB=""

HAS_SIM_LIB=false
if [ -n "$IOS_SIM_ARM64_LIB" ] && [ -f "$IOS_SIM_ARM64_LIB" ]; then
    HAS_SIM_LIB=true
fi

# Create output directories
echo ""
echo "========================================"
echo "Creating xcframework..."
echo "========================================"

mkdir -p "${OUTPUT_DIR}"
mkdir -p "${XCFRAMEWORK_DIR}/ios-arm64"
mkdir -p "${XCFRAMEWORK_DIR}/Headers"

if [ "$HAS_SIM_LIB" = true ]; then
    mkdir -p "${XCFRAMEWORK_DIR}/ios-arm64-simulator"
fi

# Copy the device library
cp "$IOS_ARM64_LIB" "${XCFRAMEWORK_DIR}/ios-arm64/libonnxruntime.a"

# Copy the simulator library (arm64 only for now)
if [ "$HAS_SIM_LIB" = true ]; then
    cp "$IOS_SIM_ARM64_LIB" "${XCFRAMEWORK_DIR}/ios-arm64-simulator/libonnxruntime.a"
else
    echo "Warning: No simulator library available"
fi

# Copy headers
HEADER_DIR=$(find build -name "onnxruntime_c_api.h" -type f | head -n 1 | xargs dirname)
if [ -n "$HEADER_DIR" ]; then
    cp "${HEADER_DIR}"/*.h "${XCFRAMEWORK_DIR}/Headers/" 2>/dev/null || true
fi

# Also copy headers from include directory
if [ -d "include/onnxruntime/core/session" ]; then
    cp include/onnxruntime/core/session/*.h "${XCFRAMEWORK_DIR}/Headers/" 2>/dev/null || true
fi

# Create Info.plist for xcframework
if [ "$HAS_SIM_LIB" = true ]; then
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
else
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
    </array>
    <key>CFBundlePackageType</key>
    <string>XFWK</string>
    <key>XCFrameworkFormatVersion</key>
    <string>1.0</string>
</dict>
</plist>
PLIST
fi

echo ""
echo "========================================"
echo "Build complete!"
echo "========================================"
echo ""
echo "ONNX Runtime xcframework created at:"
echo "  ${XCFRAMEWORK_DIR}"
echo ""
echo "Contents:"
ls -la "${XCFRAMEWORK_DIR}"
echo ""
echo "Static library sizes:"
ls -lh "${XCFRAMEWORK_DIR}/ios-arm64/libonnxruntime.a"
ls -lh "${XCFRAMEWORK_DIR}/ios-arm64-simulator/libonnxruntime.a" 2>/dev/null || echo "No simulator library"
echo ""
echo "Verifying library contains key symbols:"
nm "${XCFRAMEWORK_DIR}/ios-arm64/libonnxruntime.a" 2>/dev/null | grep -i "OrtCreateSession" | head -3 || echo "Symbols check skipped"
