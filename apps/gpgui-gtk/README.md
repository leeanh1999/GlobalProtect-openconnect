# gpgui-gtk

Open-source GTK4 / libadwaita GUI for GlobalProtect-openconnect, replacing the proprietary `gpgui`.

It drives the upstream `gpclient` CLI through `pkexec`, so all auth methods supported by
`gpclient` (password, SAML/SSO via `gpauth`, certificates, HIP) work unchanged.

## Features

- Connect / disconnect a portal (or gateway), recent portals, optional gateway selection
- Password saved in the system keyring (Secret Service)
- System tray icon (StatusNotifierItem) with connect/disconnect, close-to-tray, start minimized
- Preferences mapping to `gpclient connect` options; live log view

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
