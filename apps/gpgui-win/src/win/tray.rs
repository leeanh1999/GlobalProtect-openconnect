//! Notification-area icon: status icon + tooltip, click to show the window,
//! context menu, and balloon notifications while the window is hidden.

use std::collections::HashMap;

use gpdesktop::{controller::StatusView, state::StatusIcon};
use windows::Win32::{
  Foundation::HWND,
  UI::{
    Shell::{
      NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
      NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
    },
    WindowsAndMessaging::{DestroyIcon, GetSystemMetrics, HICON, SM_CXSMICON},
  },
};

use super::gfx::{Svg, svg_icon};

pub const TRAY_ID: u32 = 1;

pub struct Tray {
  hwnd: HWND,
  message: u32,
  added: bool,
  icons: HashMap<StatusIcon, HICON>,
  icon: StatusIcon,
  tip: String,
}

fn copy_wide(dst: &mut [u16], s: &str) {
  let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
  dst[..src.len()].copy_from_slice(&src);
  dst[src.len()] = 0;
}

impl Tray {
  pub fn new(hwnd: HWND, message: u32) -> Self {
    Self {
      hwnd,
      message,
      added: false,
      icons: HashMap::new(),
      icon: StatusIcon::Disconnected,
      tip: "GlobalProtect VPN".into(),
    }
  }

  fn hicon(&mut self, icon: StatusIcon) -> HICON {
    if let Some(h) = self.icons.get(&icon) {
      return *h;
    }
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let svg = match icon {
      StatusIcon::Connected => Svg::Connected,
      StatusIcon::Connecting => Svg::Connecting,
      StatusIcon::Disconnected => Svg::Disconnected,
      StatusIcon::Error => Svg::Error,
    };
    let h = svg_icon(svg, size).unwrap_or_default();
    self.icons.insert(icon, h);
    h
  }

  fn data(&mut self) -> NOTIFYICONDATAW {
    let icon = self.hicon(self.icon);
    let mut data = NOTIFYICONDATAW {
      cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
      hWnd: self.hwnd,
      uID: TRAY_ID,
      uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
      uCallbackMessage: self.message,
      hIcon: icon,
      ..Default::default()
    };
    copy_wide(&mut data.szTip, &self.tip);
    data
  }

  fn sync(&mut self) {
    let mut data = self.data();
    unsafe {
      if self.added {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
      } else if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        self.added = true;
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
      }
    }
  }

  pub fn update(&mut self, view: &StatusView) {
    self.icon = view.icon;
    self.tip = if view.portal.is_empty() {
      format!("GlobalProtect VPN — {}", view.title)
    } else {
      format!("GlobalProtect VPN — {} ({})", view.title, view.portal)
    };
    self.sync();
  }

  /// Explorer restarted ("TaskbarCreated"): add the icon again.
  pub fn readd(&mut self) {
    self.added = false;
    self.sync();
  }

  /// DPI or theme change: re-render icons.
  pub fn refresh_icons(&mut self) {
    for (_, h) in self.icons.drain() {
      unsafe {
        let _ = DestroyIcon(h);
      }
    }
    self.sync();
  }

  pub fn balloon(&mut self, title: &str, text: &str) {
    if !self.added {
      return;
    }
    let mut data = self.data();
    data.uFlags |= NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut data.szInfoTitle, title);
    copy_wide(&mut data.szInfo, text);
    unsafe {
      let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
  }

  pub fn remove(&mut self) {
    if self.added {
      let data = self.data();
      unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
      }
      self.added = false;
    }
  }
}

impl Drop for Tray {
  fn drop(&mut self) {
    self.remove();
    for (_, h) in self.icons.drain() {
      unsafe {
        let _ = DestroyIcon(h);
      }
    }
  }
}
