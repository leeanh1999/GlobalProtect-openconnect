<#
.SYNOPSIS
  Builds the open-source Windows package: OpenConnect (MSYS2), gpservice,
  gpauth, gpgui-win, then the NSIS installer GlobalProtectVPN-<version>-x64-setup.exe.

.DESCRIPTION
  Requirements: rustup (MSVC host), Visual Studio Build Tools with C++ (dumpbin,
  lib.exe), MSYS2 with the UCRT64 packages listed in .github/workflows/build-windows.yaml,
  NSIS 3, initialised git submodules, and Wintun 0.14.1 (downloaded and
  checksum-verified when missing).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-gpgui-win.ps1
  powershell -ExecutionPolicy Bypass -File scripts\build-gpgui-win.ps1 -SkipNative -SkipInstaller
#>
param(
  [string]$MsysRoot = 'C:\msys64',
  [ValidateSet('mingw64', 'ucrt64')][string]$MsysSubsystem = 'ucrt64',
  [string]$Prefix,
  [string]$OutDir,
  [switch]$SkipNative,
  [switch]$SkipInstaller,
  [string]$TunnelType = 'GlobalProtect VPN'
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
if (!$Prefix) { $Prefix = Join-Path $repo 'target\native-windows' }
if (!$OutDir) { $OutDir = Join-Path $repo 'target\windows-oss' }
$cargoHome = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoHome) { $env:PATH = "$cargoHome;$env:PATH" }

$version = (Select-String -Path (Join-Path $repo 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "GlobalProtect VPN $version"

# Visual Studio developer environment (dumpbin/lib for the import library, link.exe for Rust).
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vs) { throw 'Visual Studio Build Tools with the C++ workload are required.' }
$devShell = Join-Path $vs 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll'
Import-Module $devShell
Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null

# 1. OpenConnect (GnuTLS + Wintun) built with MSYS2.
$lib = Join-Path $Prefix 'lib\openconnect.lib'
if (!$SkipNative -or !(Test-Path $lib)) {
  if (!(Test-Path (Join-Path $repo 'crates\openconnect\deps\openconnect\configure.ac'))) {
    throw 'Run "git submodule update --init --recursive" first.'
  }
  & (Join-Path $PSScriptRoot 'build-openconnect-windows.ps1') -MsysRoot $MsysRoot -MsysSubsystem $MsysSubsystem -Prefix $Prefix -TunnelType $TunnelType
  if ($LASTEXITCODE) { throw "OpenConnect build failed ($LASTEXITCODE)" }
}

# 2. Rust binaries.
$env:OPENCONNECT_WINDOWS_PREFIX = $Prefix
Push-Location $repo
try {
  cargo build --release --locked -p gpservice -p gpauth -p gpgui-win
  if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)" }
} finally { Pop-Location }
$release = if ($env:CARGO_TARGET_DIR) { Join-Path $env:CARGO_TARGET_DIR 'release' } else { Join-Path $repo 'target\release' }

# 3. Wintun (vendor-signed DLL).
$tools = Join-Path $repo 'target\windows-tools'
$wintun = Join-Path $tools 'wintun\bin\amd64\wintun.dll'
if (!(Test-Path $wintun)) {
  New-Item -ItemType Directory -Force $tools | Out-Null
  $zip = Join-Path $tools 'wintun.zip'
  Invoke-WebRequest 'https://www.wintun.net/builds/wintun-0.14.1.zip' -OutFile $zip
  if ((Get-FileHash $zip).Hash -ne '07C256185D6EE3652E09FA55C0B673E2624B565E02C4B9091C79CA7D2F24EF51') { throw 'Wintun checksum mismatch.' }
  Expand-Archive -Force $zip $tools
  Remove-Item $zip
}

# 4. Payload.
$payload = Join-Path $OutDir 'payload'
if (Test-Path $payload) { Remove-Item -Recurse -Force $payload }
New-Item -ItemType Directory -Force (Join-Path $payload 'licenses') | Out-Null
foreach ($exe in 'gpservice.exe', 'gpauth.exe', 'gpgui-win.exe') { Copy-Item (Join-Path $release $exe) $payload }
Copy-Item (Join-Path $Prefix 'bin\*.dll') $payload
Copy-Item $wintun $payload
Copy-Item (Join-Path $repo 'LICENSE') (Join-Path $payload 'licenses\GPL-3.0.txt')
Copy-Item (Join-Path $repo 'packaging\windows\licenses\*') (Join-Path $payload 'licenses')
Copy-Item (Join-Path $tools 'wintun\LICENSE.txt') (Join-Path $payload 'licenses\wintun.txt')
Copy-Item (Join-Path $repo 'crates\openconnect\deps\openconnect\COPYING.LGPL') (Join-Path $payload 'licenses\openconnect-LGPL-2.1.txt')
@{ product = 'GlobalProtect VPN'; version = $version; architecture = 'x64' } |
  ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $payload 'build-info.json')
Write-Host "Payload: $payload"

# 5. Installer.
if (!$SkipInstaller) {
  $makensis = Join-Path ${env:ProgramFiles(x86)} 'NSIS\makensis.exe'
  if (!(Test-Path $makensis)) { throw 'NSIS 3 is required (makensis.exe).' }
  $setup = Join-Path $OutDir "GlobalProtectVPN-$version-x64-setup.exe"
  & $makensis /V2 /DTARGET_ARCH=x64 "/DPAYLOAD_DIR=$payload" "/DOUTPUT_FILE=$setup" "/DPRODUCT_VERSION=$version" "/DBUILD_VERSION=$version.0" (Join-Path $repo 'packaging\windows-oss\installer.nsi')
  if ($LASTEXITCODE) { throw "makensis failed ($LASTEXITCODE)" }
  Write-Host "Installer: $setup"
}
