#!/usr/bin/env bash

onnxruntime_linux_x64_archive_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "1fa4dcaef22f6f7d5cd81b28c2800414350c10116f5fdd46a2160082551c5f9b"
      ;;
    *)
      echo "No pinned Linux x64 ONNX Runtime archive SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_linux_aarch64_archive_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "7c63c73560ed76b1fac6cff8204ffe34fe180e70d6582b5332ec094810241e5c"
      ;;
    *)
      echo "No pinned Linux aarch64 ONNX Runtime archive SHA-256 for version '$1'." >&2
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

onnxruntime_macos_universal2_archive_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "49ae8e3a66ccb18d98ad3fe7f5906b6d7887df8a5edd40f49eb2b14e20885809"
      ;;
    *)
      echo "No pinned macOS universal2 ONNX Runtime archive SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_macos_universal2_dylib_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "6fee21a0dbcaa98fe082cb4f7ed07ec5def439df36198f47b61dc205e7d2a1fa"
      ;;
    *)
      echo "No pinned macOS universal2 ONNX Runtime dylib SHA-256 for version '$1'." >&2
      return 1
      ;;
  esac
}

onnxruntime_windows_x64_archive_sha256_for_version() {
  case "$1" in
    1.23.2)
      printf '%s\n' "0b38df9af21834e41e73d602d90db5cb06dbd1ca618948b8f1d66d607ac9f3cd"
      ;;
    *)
      echo "No pinned Windows x64 ONNX Runtime archive SHA-256 for version '$1'." >&2
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
