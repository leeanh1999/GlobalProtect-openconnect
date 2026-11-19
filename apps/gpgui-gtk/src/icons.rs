//! Bundled icons, embedded in the binary. GTK loads them from a private
//! icon-theme directory; the tray gets pre-rendered pixmaps.

use std::{collections::HashMap, fs, path::PathBuf};

use gtk::{gdk_pixbuf::Pixbuf, gio, glib};

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

/// Sizes offered to the tray host; it picks the closest one.
const TRAY_SIZES: [i32; 5] = [16, 22, 24, 32, 48];

/// Status icons pre-rendered as ARGB32 pixmaps, keyed by icon name.
///
/// Tray hosts (notably GNOME's AppIndicator extension) often ignore
/// `IconThemePath` and draw a "…" placeholder for unknown icon names, so the
/// tray sends pixels instead of names.
pub fn tray_pixmaps() -> HashMap<&'static str, Vec<ksni::Icon>> {
  ICONS
    .iter()
    .filter_map(|(rel, data)| {
      let name = rel.strip_prefix("hicolor/scalable/status/")?.strip_suffix(".svg")?;
      let icons: Vec<ksni::Icon> = TRAY_SIZES
        .iter()
        .filter_map(|size| match render_argb(data, *size) {
          Ok(icon) => Some(icon),
          Err(err) => {
            log::warn!("Cannot render tray icon {name}@{size}: {err}");
            None
          }
        })
        .collect();
      Some((name, icons))
    })
    .collect()
}

fn render_argb(svg: &'static [u8], size: i32) -> anyhow::Result<ksni::Icon> {
  let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(svg));
  let pixbuf = Pixbuf::from_stream_at_scale(&stream, size, size, true, gio::Cancellable::NONE)?;
  let pixbuf = if pixbuf.has_alpha() {
    pixbuf
  } else {
    pixbuf.add_alpha(false, 0, 0, 0)?
  };
  let (width, height) = (pixbuf.width(), pixbuf.height());
  let stride = pixbuf.rowstride() as usize;
  let channels = pixbuf.n_channels() as usize;
  let bytes = pixbuf.read_pixel_bytes();
  let mut data = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height as usize {
    let row = &bytes[y * stride..];
    for x in 0..width as usize {
      let px = &row[x * channels..x * channels + 4];
      // RGBA (gdk-pixbuf) -> ARGB32 in network byte order (StatusNotifierItem).
      data.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
  }
  Ok(ksni::Icon { width, height, data })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_status_icon_renders_at_every_tray_size() {
    let pixmaps = tray_pixmaps();
    for name in [
      "gpgui-gtk-connected",
      "gpgui-gtk-connecting",
      "gpgui-gtk-disconnected",
      "gpgui-gtk-error",
    ] {
      let icons = &pixmaps[name];
      assert_eq!(icons.len(), TRAY_SIZES.len(), "{name}");
      for icon in icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        assert!(icon.data.chunks(4).any(|px| px[0] > 0), "{name} is fully transparent");
      }
    }
    // Channel order: a point on the green shield (left of the check mark) must read as A,R,G,B with G dominant.
    let icon = pixmaps["gpgui-gtk-connected"].iter().find(|i| i.width == 48).unwrap();
    let (x, y) = (14usize, 18usize);
    let i = (y * icon.width as usize + x) * 4;
    let [a, r, g, b] = [icon.data[i], icon.data[i + 1], icon.data[i + 2], icon.data[i + 3]];
    assert!(a == 255 && g > r && g > b, "unexpected ARGB {a},{r},{g},{b}");
  }
}
