#!/usr/bin/env bash

# Linux and Windows take their library from Microsoft's official NuGet package.
# The package pin guards the download; the per-platform library pins below are
# what a build is held to.
onnxruntime_nuget_package_url_for_version() {
  printf '%s\n' "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/$1/microsoft.ml.onnxruntime.$1.nupkg"
}

onnxruntime_nuget_package_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "25fe172f7fcdf34f2b5b02c8b997f6b88abc282956849d4af4335af23d0c4e4a"
      ;;
    *)
      echo "No pinned ONNX Runtime NuGet package SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_linux_x64_dylib_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "13ab8084954fa4a47c777880180b90810d6020f021441395712b48a75b74c68b"
      ;;
    *)
      echo "No pinned Linux x64 ONNX Runtime shared-library SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_linux_aarch64_dylib_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "648ffa64fbe027ae27139109410900cf776a030dec2dbbac51053318cc44c286"
      ;;
    *)
      echo "No pinned Linux aarch64 ONNX Runtime shared-library SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

# macOS takes one library per architecture from the NuGet package and joins
# them. Microsoft signs these files separately from the universal library its
# release archive carried, so the code is the same but the hashes are not.
onnxruntime_macos_x64_dylib_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "8c9c78de65ea3786f987c0d980e9c1b13a3a5fbc6b3e2965ba05b450e6e4c054"
      ;;
    *)
      echo "No pinned macOS x64 ONNX Runtime dylib SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_macos_arm64_dylib_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "d306d2bc768540766c7ed8a1e0ff05d2870c77a934ebeee4a7bafa1b732ef299"
      ;;
    *)
      echo "No pinned macOS arm64 ONNX Runtime dylib SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_windows_x64_dll_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "dec964ab1ee36cc9b0ae247d13b376627992fc57dec0454354017ab8fd84f1ea"
      ;;
    *)
      echo "No pinned Windows x64 ONNX Runtime DLL SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

windows_vc_redist_x64_version() {
  printf '%s\n' "14.44.35211"
}

windows_vc_redist_x64_url_for_version() {
  case "$1" in
    14.44.35211)
      printf '%s\n' "https://download.visualstudio.microsoft.com/download/pr/7ebf5fdb-36dc-4145-b0a0-90d3d5990a61/CC0FF0EB1DC3F5188AE6300FAEF32BF5BEEBA4BDD6E8E445A9184072096B713B/VC_redist.x64.exe"
      ;;
    *)
      echo "No pinned Windows x64 VC++ Redistributable URL for version '$1'." >&2
      return 1
      ;;
  esac
}

windows_vc_redist_x64_archive_sha256_for_version() {
  case "$1" in
    14.44.35211)
      printf '%s\n' "cc0ff0eb1dc3f5188ae6300faef32bf5beeba4bdd6e8e445a9184072096b713b"
      ;;
    *)
      echo "No pinned Windows x64 VC++ Redistributable SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

windows_wix_cli_version() {
  printf '%s\n' "6.0.2"
}

windows_wix_cli_url_for_version() {
  case "$1" in
    6.0.2)
      printf '%s\n' "https://www.nuget.org/api/v2/package/wix/6.0.2"
      ;;
    *)
      echo "No pinned WiX CLI NuGet package URL for version '$1'." >&2
      return 1
      ;;
  esac
}

windows_wix_cli_archive_sha256_for_version() {
  case "$1" in
    6.0.2)
      printf '%s\n' "13caed0aa86898c9952eb8ba82c6ac6b43d1575bb731ac848e5edf5490a10428"
      ;;
    *)
      echo "No pinned WiX CLI NuGet package SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_ios_commit_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "a83fc4d58cb48eb68890dd689f94f28288cf2278"
      ;;
    *)
      echo "No pinned ONNX Runtime iOS source commit for version '$1'." >&2
      return 1
      ;;
  esac
}

# Git remotes that serve the pinned iOS source commit, tried in order. The
# commit ID pins the source and the library hashes below pin the build output,
# so a remote decides availability, never content. Microsoft's repository has
# returned 404 since 2026-11-17. Maple's fork keeps the commit under the tag
# maple-pin-v<version>, so a new pin needs a new tag there; Intel's public fork
# carries the same commit without a ref.
onnxruntime_ios_source_remotes() {
  printf '%s\n' \
    "https://github.com/microsoft/onnxruntime.git" \
    "https://github.com/MaplePrivacyLabs/onnxruntime.git" \
    "https://github.com/intel/onnxruntime.git"
}

# Fetch one commit into the current repository as FETCH_HEAD from the first
# remote that serves it. Never prompts: a missing repository asks for a login.
onnxruntime_ios_fetch_source_commit() {
  local commit="$1" remote
  while IFS= read -r remote; do
    echo "Fetching ONNX Runtime ${commit} from ${remote}"
    if GIT_TERMINAL_PROMPT=0 git fetch --depth 1 "${remote}" "${commit}" </dev/null; then
      return 0
    fi
    echo "${remote} did not serve ${commit}." >&2
  done < <(onnxruntime_ios_source_remotes)
  echo "No source remote served ONNX Runtime commit ${commit}." >&2
  return 1
}

onnxruntime_ios_device_lib_sha256_for_version() {
  local xcode_build="${2:-${MAPLE_XCODE_BUILD_VERSION:-27A9275}}"

  case "$1:${xcode_build}" in
    1.23.2:27A9275)
      printf '%s\n' "e4d288188f0b520d6676e87ad4a694326a8844618f471b5c8cfcdb69daf66657"
      ;;
    *)
      echo "No pinned ONNX Runtime iOS device library SHA-256 for version '$1' and Xcode build '${xcode_build}'." >&2
      return 1
      ;;
  esac
}

onnxruntime_ios_simulator_lib_sha256_for_version() {
  local xcode_build="${2:-${MAPLE_XCODE_BUILD_VERSION:-27A9275}}"

  case "$1:${xcode_build}" in
    1.23.2:27A9275)
      printf '%s\n' "0b670a0fce021844551d7bd9fc0c3c4275208375155e0c17254866215ae28736"
      ;;
    *)
      echo "No pinned ONNX Runtime iOS simulator library SHA-256 for version '$1' and Xcode build '${xcode_build}'." >&2
      return 1
      ;;
  esac
}

onnxruntime_ios_xcframework_sha256_for_version() {
  local xcode_build="${2:-${MAPLE_XCODE_BUILD_VERSION:-27A9275}}"

  case "$1:${xcode_build}" in
    1.23.2:27A9275)
      printf '%s\n' "93cbb699abebba06963ccf1b5c283ea0bbf7b8e9e4664f9b2df726d2808bf18a"
      ;;
    *)
      echo "No pinned ONNX Runtime iOS xcframework SHA-256 for version '$1' and Xcode build '${xcode_build}'." >&2
      return 1
      ;;
  esac
}
