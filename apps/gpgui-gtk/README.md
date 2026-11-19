# gpgui-gtk

Open-source GTK4 / libadwaita GUI for GlobalProtect-openconnect, replacing the proprietary `gpgui`.

Same architecture as the official GUI: sign-in (password, SAML/SSO via `gpauth`, MFA)
runs in the user session, then the GUI asks the root `gpservice` to open the tunnel over
its authenticated Noise/WebSocket channel. `gpservice` is started with
`pkexec gpservice --desktop-credentials`; the upstream polkit policy lets active local
users do that without a password. Internal (non-tunnel) gateways are not supported yet.

## Features

- Connect / disconnect a portal (or gateway), recent portals, optional gateway selection
- Password saved in the system keyring (Secret Service)
- System tray icon (StatusNotifierItem) with connect/disconnect, close-to-tray, start minimized
- Single-page window; connection log available from the menu (Show Log, Ctrl+L)
- Auto reconnect when an established connection drops (exponential back-off,
  optional attempt limit; toggle in the menu or Preferences)
- Detects leftover gpclient/gpauth/openconnect processes and offers to stop them
- Preferences mapping to `gpclient connect` options

Dev aid: `GPGUI_GTK_SCREENSHOT=out.png GPGUI_GTK_DEMO=error|reconnecting|log gpgui-gtk`
renders the window to a PNG and exits.

## Build

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev
cargo build --release -p gpservice -p gpclient -p gpauth -p gpgui-gtk
```

## Install

```sh
sudo sh apps/gpgui-gtk/install.sh
```

`install.sh` installs `gpservice`, `gpclient`, `gpauth`, the helpers in
`/usr/libexec/gpclient/` and the upstream polkit policy (`com.yuezk.gpgui.policy`).

## Notes

- GNOME needs the AppIndicator extension to show the tray icon.
- Quitting the app stops gpservice and therefore the VPN (close to tray to keep it running).
