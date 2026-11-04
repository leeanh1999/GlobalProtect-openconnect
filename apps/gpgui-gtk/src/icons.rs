//! Bundled icons. They are embedded in the binary and unpacked into a private
//! icon-theme directory so that both GTK and the tray host (which looks icons up
//! by name via `IconThemePath`) can find them, whether or not the app is installed.

use std::{fs, path::PathBuf};

pub const APP_ICON: &str = "io.github.gpgui_gtk.GpguiGtk";

const ICONS: &[(&str, &[u8])] = &[
  (
    "hicolor/scalable/apps/io.github.gpgui_gtk.GpguiGtk.svg",
    include_bytes!("../data/icons/hicolor/scalable/apps/io.github.gpgui_gtk.GpguiGtk.svg"),
  ),
  (
    "hicolor/scalable/status/gpgui-gtk-connected.svg",
    include_bytes!("../data/icons/hicolor/scalable/status/gpgui-gtk-connected.svg"),
  ),
  (
    "hicolor/scalable/status/gpgui-gtk-connecting.svg",
    include_bytes!("../data/icons/hicolor/scalable/status/gpgui-gtk-connecting.svg"),
  ),
  (
    "hicolor/scalable/status/gpgui-gtk-disconnected.svg",
    include_bytes!("../data/icons/hicolor/scalable/status/gpgui-gtk-disconnected.svg"),
  ),
  (
    "hicolor/scalable/status/gpgui-gtk-error.svg",
    include_bytes!("../data/icons/hicolor/scalable/status/gpgui-gtk-error.svg"),
  ),
];

/// Writes the icons (if changed) and returns the theme root, e.g. `~/.cache/gpgui-gtk/icons`.
pub fn install() -> Option<PathBuf> {
  let root = directories::ProjectDirs::from("io.github", "gpgui_gtk", "gpgui-gtk")?
    .cache_dir()
    .join("icons");
  for (rel, data) in ICONS {
    let path = root.join(rel);
    if fs::read(&path).is_ok_and(|current| current == *data) {
      continue;
    }
    let written = path
      .parent()
      .map_or(Ok(()), fs::create_dir_all)
      .and_then(|_| fs::write(&path, data));
    if let Err(err) = written {
      log::warn!("Cannot unpack icon {}: {err}", path.display());
      return None;
    }
  }
  Some(root)
}
