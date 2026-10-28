# Windows bundled runtime DLLs

Maple loads **ONNX Runtime 1.23.2** by explicit path before PDF OCR creates a
session. Windows also ships a Windows-ML
`C:\Windows\System32\onnxruntime.dll` forwarder that may be an incompatible
version, so Maple must not rely on the ambient DLL search path.

We ship the runtime **next to `maple.exe`**, and `src/onnxruntime.rs` passes
that exact path to the Rust `ort` loader. This prevents the System32 copy from
being selected accidentally.

`onnxruntime.dll` 1.23.2 in turn depends on the MSVC C++ runtime
(`VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`, `MSVCP140.dll`, `MSVCP140_1.dll`),
which is **not** present on a fresh Windows install. Rather than running
`vc_redist.x64.exe` (which needs admin/UAC and clashes with our per-user
install), we deploy those four redistributable DLLs **app-local** — the same
mechanism as `onnxruntime.dll`. (`maple.exe` itself only needs the Universal
CRT `api-ms-win-crt-*`, which is part of Windows 10+.)

Deployment is done by an NSIS install hook, `install-dlls.nsh`, registered via
`bundle.windows.nsis.installerHooks` in `tauri.windows.conf.json`. Its
`NSIS_HOOK_POSTINSTALL` `File`-copies each DLL from here into `$INSTDIR` (next
to `maple.exe`); `NSIS_HOOK_PREUNINSTALL` removes them again.

We do **not** use `bundle.resources` for this: in Tauri 2.11 a `resources` map
supplied via the platform config gets the DLLs into the installer payload but
not into the NSIS install file-list, so they ride along in the `.exe` yet never
land in the install dir (verified empirically — only `maple.exe` +
`uninstall.exe` were installed). The hook bypasses that plumbing.

## Files (5)

| File                  | Source                                              |
|-----------------------|-----------------------------------------------------|
| `onnxruntime.dll`     | Microsoft ONNX Runtime 1.23.2 win-x64 release        |
| `VCRUNTIME140.dll`    | MSVC 2015–2022 x64 redistributable                   |
| `VCRUNTIME140_1.dll`  | MSVC 2015–2022 x64 redistributable                   |
| `MSVCP140.dll`        | MSVC 2015–2022 x64 redistributable                   |
| `MSVCP140_1.dll`      | MSVC 2015–2022 x64 redistributable                   |

These are gitignored (large redistributables with their own provenance) and
must be staged here **before `bun tauri build`** on Windows.

## CI staging

The Windows PR and signed release workflows stage these automatically through
`scripts/ci/desktop-windows-pr.sh` and `scripts/ci/desktop-windows-release.sh`
before the Tauri build. Those scripts use:

- SHA-verified ONNX Runtime from `scripts/provide-windows-onnxruntime.sh`.
- A SHA-verified, versioned Microsoft `VC_redist.x64.exe` URL pinned in
  `apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh`.
- A SHA-verified WiX CLI NuGet package, used only to extract the VC++ redist
  bootstrapper payload reproducibly.

The VC++ redist contains ARM64EC payloads that report `AMD64` in the PE Machine
field. The staging script therefore rejects ARM64EC markers and only copies
native AMD64 runtime DLLs into the installer payload.

The PR build emits `target/reproducibility/desktop-pr-windows-*.sha256` proof
manifests. The signed release build emits
`target/reproducibility/desktop-release-windows-*.sha256` proof manifests after
the installer is Authenticode-signed and the final Tauri updater `.sig` is
generated. CI verifies those manifests from uploaded artifacts.

For a **local** Windows build, run `scripts/provide-windows-onnxruntime.sh`
first (it exports `ORT_DYLIB_PATH`), then run the same staging helper:

```powershell
$env:MAPLE_WINDOWS_VC_REDIST_VERSION = "14.44.35211"
$env:MAPLE_WINDOWS_VC_REDIST_URL = "<pinned URL from apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh>"
$env:MAPLE_WINDOWS_VC_REDIST_SHA256 = "<pinned SHA-256 from apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh>"
.\apps\maple-research\frontend\src-tauri\scripts\stage-windows-runtime-dlls.ps1 `
  -OrtDllPath "$env:ORT_DYLIB_PATH" `
  -Destination .\apps\maple-research\frontend\src-tauri\resources\windows
```

The hook reads these files at makensis compile time, so they must be present
here before the build runs (the CI staging step guarantees that); a missing
file fails the build loudly rather than shipping a broken installer.

## Verify after a CI build

Confirm the DLLs land **next to `maple.exe`** in the installed app
(`%LOCALAPPDATA%\Maple\`), not in a `resources\` subfolder. The running process
should load
`...\AppData\Local\Maple\onnxruntime.dll  ver 1.23.2` (check via
`(Get-Process maple).Modules`), not the System32 copy.
