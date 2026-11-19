; GlobalProtect VPN (open-source gpgui-win) installer.
; Forked from packaging/windows/installer.nsi; see README.md in this folder.
Unicode true
RequestExecutionLevel admin
!include "MUI2.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

!ifndef PAYLOAD_DIR
  !error "PAYLOAD_DIR is required"
!endif
!ifndef OUTPUT_FILE
  !error "OUTPUT_FILE is required"
!endif
!ifndef PRODUCT_VERSION
  !error "PRODUCT_VERSION is required"
!endif
!ifndef TARGET_ARCH
  !define TARGET_ARCH "x64"
!endif
!ifndef BUILD_VERSION
  !define BUILD_VERSION "${PRODUCT_VERSION}.0"
!endif

Name "GlobalProtect VPN"
OutFile "${OUTPUT_FILE}"
InstallDir "$PROGRAMFILES64\GlobalProtect VPN"
VIProductVersion "${BUILD_VERSION}"
VIAddVersionKey "ProductName" "GlobalProtect VPN"
VIAddVersionKey "FileDescription" "GlobalProtect VPN Installer"
VIAddVersionKey "ProductVersion" "${PRODUCT_VERSION}"
VIAddVersionKey "FileVersion" "${BUILD_VERSION}"
VIAddVersionKey "LegalCopyright" "GlobalProtect-openconnect contributors, GPL-3.0"
ShowInstDetails show
ShowUninstDetails show
Var InstallAction
!define MUI_ICON "${__FILEDIR__}\..\..\apps\gpgui-win\res\app.ico"
!define MUI_UNICON "${MUI_ICON}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_TITLE "GlobalProtect VPN is ready"
!define MUI_FINISHPAGE_TEXT "Open GlobalProtect VPN from the Start menu."
!define MUI_WELCOMEPAGE_TITLE "$InstallAction GlobalProtect VPN"
!define MUI_WELCOMEPAGE_TEXT "$InstallAction GlobalProtect VPN for all users on this computer.$\r$\n$\r$\nThis installs the GlobalProtect-openconnect VPN service (GPConnect) and the open-source Windows app.$\r$\n$\r$\nClick $InstallAction to continue."
!define MUI_PAGE_CUSTOMFUNCTION_SHOW ShowInstallAction
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${__FILEDIR__}\..\..\LICENSE"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
!if "${TARGET_ARCH}" == "arm64"
  ${IfNot} ${IsNativeARM64}
    MessageBox MB_OK|MB_ICONSTOP "This installer requires Windows 11 on ARM64." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
!else if "${TARGET_ARCH}" == "x64"
  ${IfNot} ${IsNativeAMD64}
    MessageBox MB_OK|MB_ICONSTOP "This installer requires x64 Windows." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
!else
  !error "Unsupported TARGET_ARCH"
!endif
  SetRegView 64
  StrCpy $InstallAction "Install"
  ReadRegStr $0 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\GlobalProtectVPN" "DisplayVersion"
  ${If} $0 != ""
    StrCpy $InstallAction "Update"
  ${EndIf}
  ; The official GP Connect registers the same Windows service.
  ReadRegStr $1 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\GPConnect" "DisplayVersion"
  ${If} $1 != ""
    MessageBox MB_OK|MB_ICONSTOP "GP Connect $1 is installed and uses the same VPN service. Uninstall it first, then run this installer again." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
FunctionEnd

Function ShowInstallAction
  GetDlgItem $0 $HWNDPARENT 1
  SendMessage $0 ${WM_SETTEXT} 0 "STR:$InstallAction"
FunctionEnd

Section "Install"
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\payload"
  File /r "${PAYLOAD_DIR}\*"
  WriteUninstaller "$PLUGINSDIR\payload\uninstall.exe"
  SetOutPath "$PLUGINSDIR"
  File "${__FILEDIR__}\machine-install.ps1"
  DetailPrint "Installing GlobalProtect VPN and its service..."
  nsExec::ExecToLog '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\machine-install.ps1" -Action Install -SourceDirectory "$PLUGINSDIR\payload" -Version "${PRODUCT_VERSION}" -ConsumeSource'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "GlobalProtect VPN could not be installed. See the details below and try again." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetRegView 64
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File "${__FILEDIR__}\machine-install.ps1"
  DetailPrint "Removing GlobalProtect VPN and its service..."
  nsExec::ExecToLog '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\machine-install.ps1" -Action Uninstall'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "GlobalProtect VPN could not be removed. See the details below and try again." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
SectionEnd
