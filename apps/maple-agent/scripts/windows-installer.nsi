; Per-user Agent installer. Product, payload and signing inputs are supplied by
; windows-release.py from checked profile metadata and the exact source tree.
Unicode true
!include "${MAPLE_CONFIG}"

Name "${DISPLAY_NAME}"
OutFile "${INSTALLER}"
Icon "${ICON}"
UninstallIcon "${ICON}"
InstallDir "$LOCALAPPDATA\Programs\${DISPLAY_NAME}"
InstallDirRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show
VIProductVersion "${NUMERIC_VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "${DISPLAY_NAME}"
VIAddVersionKey /LANG=1033 "FileDescription" "${DISPLAY_NAME} Setup"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Maple Privacy Labs"

!ifdef SIGNER
; NSIS generates a standalone uninstaller and signs it without running any
; downloaded executable. A signer error aborts the package build.
!ifdef UNSIGNED_UNINSTALLER
; Preview bundles need the same standalone PE inventory. Verification performs
; no signing and requires the compiler output to remain unsigned.
!uninstfinalize 'pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -File "${SIGNER}" "%1" -VerifyOnly -Unsigned' = 0
!else
!uninstfinalize 'pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -File "${SIGNER}" "%1"' = 0
!endif
!endif

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Var FailureCode
Var FailureStage
Var NativeStatus
Var NativeStatusAvailable
Var NativeActivationStage
Var NativeWin32Error
Var FailureAdvice

Function .onInit
  SetShellVarContext current
  ; This SYSTEM registry key is not redirected by WOW64. Built-in instructions
  ; keep the installer payload free of executable third-party UI plugins.
  StrCpy $FailureCode 10
  ClearErrors
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "PROCESSOR_ARCHITECTURE"
  IfErrors unsupported
  StrCpy $FailureCode 11
  StrCmp $0 "${NATIVE_ARCHITECTURE}" 0 unsupported
  StrCpy $FailureCode 12
  ClearErrors
  GetDLLVersion "$SYSDIR\kernel32.dll" $0 $1
  IfErrors unsupported
  StrCpy $FailureCode 13
  IntOp $0 $0 >> 16
  IntCmp $0 10 check_build unsupported supported
check_build:
  ; x64 retains its Windows 10 version 1703 floor for the OS ICU import.
  ; Native ARM64 packages target Windows 11; both floors come from package.nsh.
  ; GetDLLVersion's second DWORD contains the build in its high 16 bits.
  IntOp $1 $1 >> 16
  IntOp $1 $1 & 0xFFFF
  IntCmp $1 ${MIN_WINDOWS_BUILD} supported unsupported supported
unsupported:
  MessageBox MB_OK|MB_ICONSTOP "${DISPLAY_NAME} requires ${WINDOWS_REQUIREMENT}." /SD IDOK
  SetErrorLevel $FailureCode
  Abort
supported:
FunctionEnd

Var PreviousInstallLocation
Var PreviousDisplayVersion
Var HadRegistration

Section "Install"
  StrCpy $NativeStatus 0
  StrCpy $NativeStatusAvailable 0
  StrCpy $NativeActivationStage 0
  ; The temporary copy is the same verified signed application PE. Its private
  ; early command performs no GUI, account, store or backend initialization.
  ; All destination/DACL/reparse checks happen before any installed file or
  ; registry changes. Unsigned Prod previews deliberately stop here.
  StrCpy $FailureCode 20
  StrCpy $FailureStage "extracting the installation checker"
  SetErrorLevel 20
  ClearErrors
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File /oname=maple-installer-check.exe "${PAYLOAD}\maple-agent.exe"
  IfErrors refused
  StrCpy $FailureCode 21
  StrCpy $FailureStage "starting installation checks"
  StrCpy $NativeStatusAvailable 0
  ClearErrors
  ExecWait '"$PLUGINSDIR\maple-installer-check.exe" --internal-windows-installer-preflight "$INSTDIR"' $NativeStatus
  IfErrors refused
  StrCpy $NativeStatusAvailable 1
  StrCpy $FailureCode 22
  StrCpy $FailureStage "checking the installation"
  StrCmp $NativeStatus 0 0 refused
  ; The signed helper durably records the manual operation, creates a private
  ; root, bootstraps only a fresh install and repairs incomplete non-current
  ; generation files. A retry must never overwrite a live or valid rollback
  ; generation, nor remain stuck with an interrupted NSIS file copy.
  StrCpy $FailureCode 23
  StrCpy $FailureStage "starting installation preparation"
  StrCpy $NativeStatusAvailable 0
  ClearErrors
  ExecWait '"$PLUGINSDIR\maple-installer-check.exe" --internal-windows-installer-prepare "$INSTDIR" ${GENERATION}' $NativeStatus
  IfErrors refused
  StrCpy $NativeStatusAvailable 1
  StrCpy $FailureCode 24
  StrCpy $FailureStage "preparing the installation"
  StrCmp $NativeStatus 0 0 refused

  StrCpy $FailureCode 31
  StrCpy $FailureStage "checking installation registration"
  StrCpy $NativeStatusAvailable 0
  StrCpy $HadRegistration 0
  ClearErrors
  ReadRegStr $PreviousInstallLocation HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation"
  IfErrors unregistered
  StrCmp $PreviousInstallLocation "$INSTDIR" 0 refused
  StrCpy $HadRegistration 1
  ReadRegStr $PreviousDisplayVersion HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayVersion"
unregistered:
  ; Never overwrite the stable bootstrap or an existing generation. All new
  ; generation bytes arrive before the native helper commits current.json.
  StrCpy $FailureCode 30
  StrCpy $FailureStage "copying application files"
  SetOutPath "$INSTDIR"
  IfFileExists "$INSTDIR\maple-agent.exe" bootstrap_ready
  ClearErrors
  File "${PAYLOAD}\maple-agent.exe"
  IfErrors failed
bootstrap_ready:
  SetOutPath "$INSTDIR\versions\${GENERATION}"
  SetOverwrite off
  ClearErrors
  File "${PAYLOAD}\maple-agent.exe"
  File "${PAYLOAD}\LICENSE.txt"
  File "${PAYLOAD}\build-info.json"
  SetOutPath "$INSTDIR\versions\${GENERATION}"
  File "${PAYLOAD}\bundle-info.json"
  IfErrors failed
  SetOverwrite on
  SetOutPath "$INSTDIR"
  IfFileExists "$INSTDIR\uninstall.exe" uninstaller_ready
  WriteUninstaller "$INSTDIR\uninstall.exe"
  IfErrors failed
uninstaller_ready:
  StrCpy $FailureCode 31
  StrCpy $FailureStage "registering the application"
  CreateDirectory "$SMPROGRAMS\${DISPLAY_NAME}"
  CreateShortcut "$SMPROGRAMS\${DISPLAY_NAME}\${DISPLAY_NAME}.lnk" "$INSTDIR\maple-agent.exe"
  CreateShortcut "$SMPROGRAMS\${DISPLAY_NAME}\Uninstall.lnk" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayName" "${DISPLAY_NAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "Publisher" "Maple Privacy Labs"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "QuietUninstallString" '$\"$INSTDIR\uninstall.exe$\" /S'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayIcon" '$\"$INSTDIR\maple-agent.exe$\",0'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoRepair" 1
  IfErrors failed
  StrCpy $FailureCode 40
  StrCpy $FailureStage "starting installation activation"
  StrCpy $NativeStatusAvailable 0
  ClearErrors
  ExecWait '"$INSTDIR\maple-agent.exe" --internal-windows-installer-activate ${GENERATION}' $NativeStatus
  IfErrors failed
  StrCpy $NativeStatusAvailable 1
  StrCpy $FailureCode 41
  StrCpy $FailureStage "activating the installation"
  StrCmp $NativeStatus 0 0 failed
  SetErrorLevel 0
  Goto done
failed:
  ; Native activation diagnostics come from this exact ExecWait child. Keep
  ; ordinary failures at 41; only the fixed marker and stages 1..10 can carry
  ; an activation stage plus its bounded Win32 error to an unattended caller.
  StrCmp $FailureCode 41 0 activation_diagnostic_done
  IntOp $1 $NativeStatus & 0xFF000000
  IntCmp $1 0x4D000000 0 activation_diagnostic_done activation_diagnostic_done
  IntOp $1 $NativeStatus >> 16
  IntOp $1 $1 & 0xFF
  IntCmp $1 1 0 activation_diagnostic_done 0
  IntCmp $1 10 0 0 activation_diagnostic_done
  StrCpy $NativeActivationStage $1
  IntOp $NativeWin32Error $NativeStatus & 0xFFFF
  StrCpy $FailureCode $NativeStatus
activation_diagnostic_done:
  ; The native activation failure leaves the old pointer unchanged. Restore
  ; registration truth; user data and an earlier generation remain untouched.
  StrCmp $HadRegistration 1 restore_registration
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  Goto refused
restore_registration:
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation" "$PreviousInstallLocation"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayVersion" "$PreviousDisplayVersion"
refused:
  StrCpy $1 "Installation stopped while $FailureStage.$\r$\n$\r$\nInstaller stage: $FailureCode"
  StrCmp $NativeStatusAvailable 1 0 show_error
  IntFmt $2 "0x%08X" $NativeStatus
  StrCpy $1 "$1$\r$\nWindows process exit code: $2"
  StrCmp $NativeActivationStage 0 describe_error
  StrCpy $1 "$1$\r$\nNative activation stage: $NativeActivationStage"
  StrCmp $NativeWin32Error 0 describe_error
  StrCpy $1 "$1$\r$\nWindows error: $NativeWin32Error"
describe_error:
  Call DescribeInstallerError
  StrCpy $1 "$1$\r$\n$\r$\n$FailureAdvice"
show_error:
  MessageBox MB_OK|MB_ICONSTOP "$1$\r$\n$\r$\nPlease include these details when contacting Maple support." /SD IDOK
  SetErrorLevel $FailureCode
  Abort
done:
SectionEnd

Function DescribeInstallerError
  StrCpy $FailureAdvice "Windows could not complete this installation."
  StrCpy $2 $NativeStatus
  StrCmp $NativeActivationStage 0 classify
  StrCpy $2 $NativeWin32Error
classify:
  IntCmp $2 0x800B0109 untrusted
  IntCmp $2 0x800B010A untrusted
  IntCmp $2 0x80092013 revocation
  IntCmp $2 0x80092012 revocation
  IntCmp $2 0x800B0101 certificate_date
  IntCmp $2 0x80096010 signature
  IntCmp $2 0x800B0100 signature
  IntCmp $2 0x4E000001 signature
  IntCmp $2 0x4E000002 busy
  IntCmp $2 0x4E000003 validation
  IntCmp $2 0x4E000004 manual
  IntCmp $2 0x4E000005 storage
  IntCmp $2 5 access
  IntCmp $2 32 locked
  IntCmp $2 33 locked
  IntCmp $2 0xC0000135 component
  Goto done
untrusted:
  StrCpy $FailureAdvice "Windows could not establish trust in the signing certificate. Check Windows Update and internet access, then retry."
  Goto done
revocation:
  StrCpy $FailureAdvice "Windows could not check the signing certificate's revocation status. Check your internet connection, then retry."
  Goto done
certificate_date:
  StrCpy $FailureAdvice "Windows reported a certificate validity problem. Check the computer's date and time and Windows Update."
  Goto done
signature:
  StrCpy $FailureAdvice "The application signature or identity could not be verified. Download a new installer from Maple's official download page."
  Goto done
busy:
  StrCpy $FailureAdvice "Another Maple installation or update is active. Let it finish before retrying."
  Goto done
validation:
  StrCpy $FailureAdvice "The installation folder or application files did not pass validation. For a first installation, choose a new empty folder. Otherwise contact Maple support."
  Goto done
manual:
  StrCpy $FailureAdvice "Maple could not safely replace this installation. Contact Maple support with these details."
  Goto done
storage:
  StrCpy $FailureAdvice "Windows could not read or write installation files. Contact Maple support with these details."
  Goto done
access:
  StrCpy $FailureAdvice "Windows denied access to the installation folder. Choose a folder owned by your user account, or contact Maple support."
  Goto done
locked:
  StrCpy $FailureAdvice "An installation file is in use. Close ${DISPLAY_NAME} and its background host, then retry."
  Goto done
component:
  StrCpy $FailureAdvice "Windows could not load a required system component. Check Windows Update and contact Maple support with this code."
done:
FunctionEnd

Function .onInstSuccess
  ; The success callback runs when the completed installer closes. Silent
  ; updates/reinstalls stay quiet, and failed or cancelled installs never call
  ; this callback. The stable launcher verifies the committed generation.
  IfSilent done
  SetOutPath "$INSTDIR"
  ClearErrors
  Exec '"$INSTDIR\maple-agent.exe"'
  IfErrors 0 done
  MessageBox MB_OK|MB_ICONEXCLAMATION "${DISPLAY_NAME} was installed, but could not be opened. Open it from the Start menu."
done:
FunctionEnd

Section "Uninstall"
  SetShellVarContext current
  StrCpy $FailureCode 50
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation"
  StrCmp $0 "$INSTDIR" 0 refuse
  ; The trusted bootstrap derives its actual root and removes fixed owned
  ; generation files only after signature, lock, DACL and reparse checks.
  ; It preserves unknown user files, AppData and profile/database state.
  StrCpy $FailureCode 51
  ClearErrors
  ExecWait '"$INSTDIR\maple-agent.exe" --internal-windows-uninstall-cleanup' $0
  IfErrors refuse
  StrCpy $FailureCode 52
  StrCmp $0 0 0 refuse
  StrCpy $FailureCode 53
  ClearErrors
  Delete "$INSTDIR\current.json"
  ; The helper has exited and released this exact private lock. Removing only
  ; its fixed transaction files permits a later clean reinstall at this root.
  IfFileExists "$INSTDIR\.install.lock" 0 lock_removed
  Delete "$INSTDIR\.install.lock"
lock_removed:
  IfFileExists "$INSTDIR\.installer-intent.json" 0 intent_removed
  Delete "$INSTDIR\.installer-intent.json"
intent_removed:
  Delete "$INSTDIR\maple-agent.exe"
  IfErrors refuse
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR\versions"
  Delete "$SMPROGRAMS\${DISPLAY_NAME}\${DISPLAY_NAME}.lnk"
  Delete "$SMPROGRAMS\${DISPLAY_NAME}\Uninstall.lnk"
  RMDir "$SMPROGRAMS\${DISPLAY_NAME}"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  ; The durable fence belongs to this NSIS process. Keep it while performing
  ; all installed-file, shortcut and registration changes, after the native
  ; helper releases its handles. No root/registration mutation may follow its
  ; removal. Leave the empty root so a concurrently admitted reinstall cannot
  ; lose its destination between directory creation and publishing its intent.
  Delete "$INSTDIR\.uninstaller-intent.json"
  SetErrorLevel 0
  Goto done
refuse:
  MessageBox MB_OK|MB_ICONSTOP "Close ${DISPLAY_NAME} and its background host, then uninstall from its registered installed location." /SD IDOK
  SetErrorLevel $FailureCode
  Abort
done:
SectionEnd
