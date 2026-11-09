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
; NSIS signs the uninstaller it generates. A signer error aborts the package build.
!uninstfinalize 'pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -File "${SIGNER}" -File "%1"' = 0
!endif

; No folder page: the per-user location under the profile is the only one the
; native installer can protect. A folder elsewhere, such as under a drive root,
; inherits write access for every signed-in user. A previously registered
; location (InstallDirRegKey) and /D= still apply.
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Var FailureCode
Var FailureStage
Var NativeStatus
Var NativeStatusAvailable
Var NativeActivationStage
Var NativeWin32Error
Var InstallerReport
Var ReportPath
Var FailureAdvice
Var ExitCode
Var StagingRoot
Var Staging

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

Section "Install"
  StrCpy $NativeStatus 0
  StrCpy $NativeStatusAvailable 0
  StrCpy $NativeActivationStage 0
  StrCpy $NativeWin32Error 0
  StrCpy $InstallerReport ""
  StrCpy $Staging ""
  InitPluginsDir
  ; The payload is unpacked into a folder this installer creates with its own
  ; protected ACL, so the native installer's permission checks see only the
  ; entries Maple set. Under %TEMP%, a moved TEMP such as C:\Temp or a profile
  ; with extra entries made every payload file writable by other accounts.
  StrCpy $FailureCode 21
  StrCpy $StagingRoot "$LOCALAPPDATA\Maple Work Installer Staging"
  StrCpy $0 $StagingRoot
  Call CreatePrivateDirectory
  StrCmp $1 0 staging_root
  StrCmp $1 183 staging_root
  StrCpy $FailureStage "creating a private folder for the installer files (Windows error $1)"
  Goto refused
staging_root:
  ; Each run gets a new folder; an existing one is never adopted.
  System::Call 'kernel32::GetCurrentProcessId() i .r5'
  System::Call 'kernel32::GetTickCount() i .r6'
  IntFmt $6 "%u" $6
  StrCpy $0 "$StagingRoot\$5-$6"
  Call CreatePrivateDirectory
  StrCmp $1 0 staging_created
  StrCpy $FailureStage "creating a private folder for the installer files (Windows error $1)"
  Goto refused
staging_created:
  StrCpy $Staging $0
  StrCpy $FailureCode 20
  StrCpy $FailureStage "extracting the installer payload"
  ClearErrors
  SetOutPath "$Staging\payload"
  File "${PAYLOAD}\maple-agent.exe"
  File "${PAYLOAD}\LICENSE.txt"
  File "${PAYLOAD}\build-info.json"
  File "${PAYLOAD}\bundle-info.json"
  WriteUninstaller "$Staging\payload\uninstall.exe"
  IfErrors refused
  ; The native child owns admission, locks, copying, registration, shortcuts
  ; and activation. NSIS never mutates the installed root between phases.
  ; A failed transaction is resumed only by a new installer process: the
  ; child refuses while the recorded owner, this process, is still alive.
  StrCpy $FailureCode 23
  StrCpy $FailureStage "starting the native installer"
  StrCpy $ReportPath "$Staging\install-report.txt"
  ClearErrors
  ExecWait '"$Staging\payload\maple-agent.exe" --internal-windows-installer-install "$INSTDIR" ${GENERATION} --internal-windows-installer-report "$ReportPath"' $NativeStatus
  IfErrors refused
  Call ReadInstallerReport
  StrCpy $NativeStatusAvailable 1
  StrCpy $FailureCode 24
  StrCpy $FailureStage "installing the application"
  StrCmp $NativeStatus 0 done
  ; Keep legacy activation status decoding for unattended installer callers.
  IntOp $1 $NativeStatus & 0xFF000000
  IntCmp $1 0x4D000000 0 refused refused
  IntOp $1 $NativeStatus >> 16
  IntOp $1 $1 & 0xFF
  IntCmp $1 1 0 refused 0
  IntCmp $1 10 0 0 refused
  StrCpy $NativeActivationStage $1
  IntOp $NativeWin32Error $NativeStatus & 0xFFFF
refused:
  Call RemoveStaging
  ; Unattended callers get the full native status for an activation failure
  ; and the installer stage otherwise. The dialog always shows the stage.
  StrCpy $ExitCode $FailureCode
  StrCmp $NativeActivationStage 0 exit_code_chosen
  StrCpy $ExitCode $NativeStatus
exit_code_chosen:
  StrCpy $FailureAdvice ""
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
  ; The native report replaces the summary above but keeps its advice, which
  ; is the only part that tells the user what to do.
  StrCmp $InstallerReport "" no_report
  IntFmt $4 "0x%08X" $NativeStatus
  StrCpy $1 "${DISPLAY_NAME} ${VERSION}, build ${BUILD}.$\r$\nInstallation stopped while $FailureStage.$\r$\n"
  StrCmp $FailureAdvice "" report_status
  StrCpy $1 "$1$FailureAdvice$\r$\n"
report_status:
  StrCpy $1 "$1$\r$\nStage: $FailureCode; status: $4$\r$\n$\r$\n$InstallerReport"
no_report:
  MessageBox MB_OK|MB_ICONSTOP "$1$\r$\n$\r$\nPress Ctrl+C to copy this message for Maple support." /SD IDOK
  SetErrorLevel $ExitCode
  Abort
done:
  Call RemoveStaging
  SetErrorLevel 0
SectionEnd

; Create the folder named by $0 with a protected ACL: full control for SYSTEM,
; Administrators and the folder's owner, the installing user, and nothing
; inherited from the parent. Files written into it inherit only these entries.
; Sets $1 to 0 on success or to the Windows error (183: it already exists).
; Uses $2 to $4. System is NSIS's own plugin; CreateDirectoryW applies the ACL
; atomically, so the folder never exists with its parent's entries.
Function CreatePrivateDirectory
  StrCpy $1 -1
  System::Call 'advapi32::ConvertStringSecurityDescriptorToSecurityDescriptorW(w "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)", i 1, *p .r2, p 0) i .r3 ?e'
  Pop $1
  StrCmp $3 0 done
  System::Call '*(&l4, p r2, i 0) p .r4'
  System::Call 'kernel32::CreateDirectoryW(w r0, p r4) i .r3 ?e'
  Pop $1
  StrCmp $3 0 free
  StrCpy $1 0
free:
  System::Free $4
  System::Call 'kernel32::LocalFree(p r2)'
done:
FunctionEnd

; Delete this run's staging folder, and the shared root when it is empty.
Function RemoveStaging
  StrCmp $Staging "" done
  ; The output path is the process's current folder; leave it first.
  SetOutPath "$PLUGINSDIR"
  RMDir /r "$Staging"
  RMDir "$StagingRoot"
  StrCpy $Staging ""
done:
FunctionEnd

; This file is diagnostic text only; it never authorizes a successful install.
Function ReadInstallerReport
  StrCpy $InstallerReport ""
  ClearErrors
  FileOpen $0 "$ReportPath" r
  IfErrors done
loop:
  ClearErrors
  FileRead $0 $3
  IfErrors close
  DetailPrint "$3"
  StrCpy $InstallerReport "$InstallerReport$3"
  Goto loop
close:
  FileClose $0
done:
FunctionEnd

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
  StrCpy $FailureAdvice "The installation folder or application files did not pass validation. Copy these details for Maple support."
  Goto done
manual:
  StrCpy $FailureAdvice "Maple could not safely replace this installation. Contact Maple support with these details."
  Goto done
storage:
  StrCpy $FailureAdvice "Maple could not validate installation storage. Copy these details for Maple support."
  Goto done
access:
  StrCpy $FailureAdvice "Windows denied access to the installation folder. Copy these details for Maple support."
  Goto done
locked:
  StrCpy $FailureAdvice "An installation file is in use, usually because ${DISPLAY_NAME} is still running. Close ${DISPLAY_NAME}, wait a few seconds for its background host to stop, then run this installer again."
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
