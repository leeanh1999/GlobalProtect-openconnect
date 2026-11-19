#!/bin/sh
# Install gpgui-gtk with the upstream service stack from target/release into /usr.
# Usage (from repo root or an unpacked release): sudo sh apps/gpgui-gtk/install.sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
REL="$ROOT/target/release"
DATA="$ROOT/apps/gpgui-gtk/data"
FILES="$ROOT/packaging/files/usr"

for bin in gpservice gpclient gpauth gpgui-gtk gp-vpnc-script-installer gp-hip-script-installer; do
  test -x "$REL/$bin" || { echo "Missing $REL/$bin — run: cargo build --release -p gpservice -p gpclient -p gpauth -p gpgui-gtk" >&2; exit 1; }
done

for bin in gpservice gpclient gpauth gpgui-gtk; do
  install -Dm755 "$REL/$bin" "/usr/bin/$bin"
done
for bin in gp-vpnc-script-installer gp-hip-script-installer; do
  install -Dm755 "$REL/$bin" "/usr/libexec/gpclient/$bin"
done
install -Dm755 "$FILES/libexec/gpclient/vpnc-script" /usr/libexec/gpclient/vpnc-script

# Upstream polkit policy: active local users may start gpservice without a password
# (it only accepts authenticated requests from the GUI that launched it).
install -Dm644 "$FILES/share/polkit-1/actions/com.yuezk.gpgui.policy" /usr/share/polkit-1/actions/com.yuezk.gpgui.policy
# Policy from gpgui-gtk < 0.3 (pkexec gpclient) is no longer used.
rm -f /usr/share/polkit-1/actions/io.github.gpgui_gtk.gpclient.policy

install -Dm644 "$DATA/io.github.gpgui_gtk.GpguiGtk.desktop" /usr/share/applications/io.github.gpgui_gtk.GpguiGtk.desktop
for icon in $(cd "$DATA/icons" && find . -name '*.svg'); do
  install -Dm644 "$DATA/icons/$icon" "/usr/share/icons/$icon"
done
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true

echo "Installed. Launch 'GlobalProtect VPN' from the app menu or run: gpgui-gtk"
