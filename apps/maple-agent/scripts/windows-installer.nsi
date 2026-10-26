; Per-user Agent installer. Product, payload and signing inputs are supplied by
; windows-release.py from checked profile metadata and the exact source tree.
Unicode true
!include "${MAPLE_CONFIG}"

Name "${DISPLAY_NAME}"
OutFile "${INSTALLER}"
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
!uninstfinalize 'pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -File "${SIGNER}" "%1"' = 0
!endif

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Function .onInit
  SetShellVarContext current
  ; This SYSTEM registry key is not redirected by WOW64. Built-in instructions
  ; keep the installer payload free of executable third-party UI plugins.
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "PROCESSOR_ARCHITECTURE"
  StrCmp $0 "AMD64" 0 unsupported
  GetDLLVersion "$SYSDIR\kernel32.dll" $0 $1
  IntOp $0 $0 >> 16
  IntCmp $0 10 supported unsupported supported
unsupported:
  MessageBox MB_OK|MB_ICONSTOP "${DISPLAY_NAME} requires Windows 10 or later on an x64 computer." /SD IDOK
  SetErrorLevel 1
  Abort
supported:
FunctionEnd

Section "Install"
  ; Fail before changing any installed files if the desktop or background host
  ; still has the executable open. Append mode preserves the existing bytes;
  ; opening for write fails while Windows maps the running executable.
  IfFileExists "$INSTDIR\maple-agent.exe" 0 ready
  ClearErrors
  FileOpen $0 "$INSTDIR\maple-agent.exe" a
  IfErrors locked
  FileClose $0
  Goto ready
locked:
  MessageBox MB_OK|MB_ICONSTOP "Close ${DISPLAY_NAME} and its background host before installing." /SD IDOK
  SetErrorLevel 1
  Abort
ready:
  SetOutPath "$INSTDIR"
  SetOverwrite on
  ClearErrors
  File "${PAYLOAD}\maple-agent.exe"
  IfErrors failed
  File "${PAYLOAD}\LICENSE.txt"
  File "${PAYLOAD}\build-info.json"
  SetOutPath "$INSTDIR\notices"
  File "${PAYLOAD}\notices\THIRD_PARTY_NOTICES.txt"
  File "${PAYLOAD}\notices\THIRD_PARTY_NOTICES.html"
  File "${PAYLOAD}\notices\manifest.json"
  SetOutPath "$INSTDIR\notices\wordlists"
  File "${PAYLOAD}\notices\wordlists\eff-large-wordlist.txt"
  File "${PAYLOAD}\notices\wordlists\SOURCES.md"
  IfErrors failed
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  IfErrors failed
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
  SetErrorLevel 0
  Goto done
failed:
  MessageBox MB_OK|MB_ICONSTOP "Installation could not replace every application file. Close the application and retry." /SD IDOK
  SetErrorLevel 1
  Abort
done:
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  ; A copied uninstaller must never remove files from an arbitrary directory.
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "InstallLocation"
  StrCmp $0 "$INSTDIR" 0 refuse
  ClearErrors
  Delete "$INSTDIR\maple-agent.exe"
  IfErrors refuse
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\build-info.json"
  Delete "$INSTDIR\notices\THIRD_PARTY_NOTICES.txt"
  Delete "$INSTDIR\notices\THIRD_PARTY_NOTICES.html"
  Delete "$INSTDIR\notices\manifest.json"
  Delete "$INSTDIR\notices\wordlists\eff-large-wordlist.txt"
  Delete "$INSTDIR\notices\wordlists\SOURCES.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR\notices\wordlists"
  RMDir "$INSTDIR\notices"
  ; Remove empty application directories only; user-created files and all
  ; configuration/data under AppData remain untouched.
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${DISPLAY_NAME}\${DISPLAY_NAME}.lnk"
  Delete "$SMPROGRAMS\${DISPLAY_NAME}\Uninstall.lnk"
  RMDir "$SMPROGRAMS\${DISPLAY_NAME}"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  SetErrorLevel 0
  Goto done
refuse:
  MessageBox MB_OK|MB_ICONSTOP "Close ${DISPLAY_NAME} and its background host, then uninstall from its installed location." /SD IDOK
  SetErrorLevel 1
  Abort
done:
SectionEnd
