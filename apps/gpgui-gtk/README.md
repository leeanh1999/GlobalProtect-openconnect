# gpgui-gtk

Open-source GTK4 / libadwaita GUI for GlobalProtect-openconnect, replacing the proprietary `gpgui`.

It drives the upstream `gpclient` CLI through `pkexec`, so all auth methods supported by
`gpclient` (password, SAML/SSO via `gpauth`, certificates, HIP) work unchanged.

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
cargo build --release -p gpgui-gtk
```

## Install

```sh
sudo install -Dm755 target/release/gpgui-gtk /usr/bin/gpgui-gtk
sudo install -Dm644 apps/gpgui-gtk/data/io.github.gpgui_gtk.GpguiGtk.desktop /usr/share/applications/
sudo install -Dm644 apps/gpgui-gtk/data/io.github.gpgui_gtk.gpclient.policy /usr/share/polkit-1/actions/
```

`gpclient` and `gpauth` must be installed (`/usr/bin/gpclient`, or set the path in Preferences;
the polkit policy assumes `/usr/bin/gpclient`).

## Notes

- GNOME needs the AppIndicator extension to show the tray icon.
- Quitting while connected disconnects first, because `gpclient` logs to the GUI's pipes.
