#!/bin/sh
# Install gpclient, gpauth and gpgui-gtk from target/release into /usr.
# Usage (from repo root): sudo sh apps/gpgui-gtk/install.sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
REL="$ROOT/target/release"
DATA="$ROOT/apps/gpgui-gtk/data"

for bin in gpclient gpauth gpgui-gtk; do
  test -x "$REL/$bin" || { echo "Missing $REL/$bin — run: cargo build --release -p gpclient -p gpauth -p gpgui-gtk" >&2; exit 1; }
done

install -Dm755 "$REL/gpclient"  /usr/bin/gpclient
install -Dm755 "$REL/gpauth"    /usr/bin/gpauth
install -Dm755 "$REL/gpgui-gtk" /usr/bin/gpgui-gtk
install -Dm755 "$ROOT/packaging/files/usr/libexec/gpclient/vpnc-script" /usr/libexec/gpclient/vpnc-script
install -Dm644 "$DATA/io.github.gpgui_gtk.GpguiGtk.desktop" /usr/share/applications/io.github.gpgui_gtk.GpguiGtk.desktop
install -Dm644 "$DATA/io.github.gpgui_gtk.gpclient.policy"  /usr/share/polkit-1/actions/io.github.gpgui_gtk.gpclient.policy
for icon in $(cd "$DATA/icons" && find . -name '*.svg'); do
  install -Dm644 "$DATA/icons/$icon" "/usr/share/icons/$icon"
done
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true

echo "Installed. Launch 'GlobalProtect VPN' from the app menu or run: gpgui-gtk"
