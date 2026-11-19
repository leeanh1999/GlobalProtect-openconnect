# gpgui-win — GlobalProtect VPN for Windows

Open-source native Windows GUI for GlobalProtect-openconnect (Win32 + Direct2D/DirectWrite),
the Windows counterpart of `gpgui-gtk`. It replaces the proprietary GP Connect GUI.

Architecture (same as the official GUI): sign-in (password, SAML/SSO via `gpauth`, MFA)
runs in the user session; the `GPConnect` Windows service (`gpservice`, LocalSystem) opens
the tunnel. The GUI obtains a session credential from the service's admission pipe
(`\\.\pipe\GPConnect.Service`), keeps the pipe open as its lease, and talks to the service
over the Noise-encrypted WebSocket. Quitting the app does **not** stop the service or the
tunnel; reopening it shows the live connection.

All connection logic lives in `crates/gpdesktop` (shared, UI-agnostic); this crate only
renders it.

## Features (parity with gpgui-gtk)

- Connect / disconnect a portal or gateway, recent portals, optional gateway
- SAML/SSO (embedded WebView2 window or default browser), username/password, MFA codes
- Password saved in Windows Credential Manager (`GlobalProtect VPN:<user>@<portal>`)
- Notification-area icon with status, Connect/Disconnect, Show Window, Quit; close to tray,
  start minimized (`--minimized`), balloon messages while hidden
- Auto reconnect with exponential back-off and optional attempt limit; re-authentication
  when the service asks for it
- Connection log (menu › Show Log, Ctrl+L) with Copy / Clear
- Preferences mirroring `gpclient connect` options
- Detects other VPN processes (gpclient, gpauth, openconnect) and offers to stop them. The
  Palo Alto GlobalProtect agent is never touched; disconnect it yourself before connecting.
- Light/dark theme following Windows, per-monitor DPI

Not available: HIP posture on Windows needs the proprietary `gp-posture.exe` (not shipped);
internal (non-tunnel) gateways are not supported yet (same as gpgui-gtk).

## Build

Prerequisites: rustup (MSVC), Visual Studio Build Tools (C++), MSYS2 (UCRT64 packages from
`.github/workflows/build-windows.yaml`), NSIS 3, `git submodule update --init --recursive`.

```powershell
# GUI only (talks to an installed service)
cargo build -p gpgui-win

# Full package: OpenConnect, gpservice, gpauth, gpgui-win and the installer
powershell -ExecutionPolicy Bypass -File scripts\build-gpgui-win.ps1
# -> target\windows-oss\GlobalProtectVPN-<version>-<x64|arm64>-setup.exe
```

The script builds for the host architecture: x64 uses MSYS2 UCRT64, ARM64 (Windows 11 on ARM)
uses MSYS2 CLANGARM64 and the Visual Studio ARM64 tools.

The installer (`packaging/windows-oss`) registers the same `GPConnect` service as GP Connect,
so it refuses to install while GP Connect is installed. Binaries are not code-signed.

## Dev aids

```powershell
# Render a window to PNG and exit (no service needed)
$env:GPGUI_WIN_SCREENSHOT='out.png'; $env:GPGUI_WIN_DEMO='connected'   # form|connecting|reconnecting|error|toast|log|prefs|otp|about|quit
$env:GPGUI_WIN_THEME='dark'; $env:GPGUI_WIN_SCALE='1.5'
.\target\debug\gpgui-win.exe

# Regenerate res\app.ico from res\icons\app.svg
.\target\debug\gpgui-win.exe --export-icons apps\gpgui-win\res

# Headless protocol smoke test against the installed service
cargo run -p gpdesktop --example connect -- vpn.example.com [username] [gateway]
```
