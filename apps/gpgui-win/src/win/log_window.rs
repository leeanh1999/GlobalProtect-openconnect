//! Connection Log window (menu › Show Log, Ctrl+L): a read-only native
//! multi-line EDIT with Copy and Clear actions.

use std::collections::VecDeque;

use gpdesktop::LogLevel;
use windows::{
  Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    Graphics::Gdi::{
      CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DeleteObject, FF_DONTCARE, FW_NORMAL,
      HFONT, OUT_DEFAULT_PRECIS,
    },
    UI::{
      Controls::SetWindowTheme,
      WindowsAndMessaging::{
        CreateWindowExW, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GetWindowTextLengthW, SWP_NOACTIVATE, SWP_NOZORDER,
        SendMessageW, SetWindowPos, SetWindowTextW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT, WM_VSCROLL, WS_CHILD,
        WS_VISIBLE, WS_VSCROLL,
      },
    },
  },
  core::{HSTRING, PCWSTR, w},
};

use super::{
  Effect,
  gfx::{Canvas, rect},
  scene::{ButtonStyle, Id, Kind, Surface},
  theme::Theme,
};

pub const WIDTH: f32 = 720.0;
pub const HEIGHT: f32 = 480.0;
pub const MAX_LINES: usize = 2000;

const ID_COPY: Id = 1;
const ID_CLEAR: Id = 2;
const TOOLBAR: f32 = 52.0;

/// Log lines kept while the window is closed.
#[derive(Default)]
pub struct LogBuffer {
  lines: VecDeque<String>,
}

impl LogBuffer {
  pub fn push(&mut self, level: LogLevel, message: &str) -> String {
    let prefix = match level {
      LogLevel::Error => "ERROR ",
      LogLevel::Warn => "WARN  ",
      LogLevel::Info => "",
      LogLevel::Debug => "DEBUG ",
    };
    let line = format!("{prefix}{message}");
    self.lines.push_back(line.clone());
    while self.lines.len() > MAX_LINES {
      self.lines.pop_front();
    }
    line
  }

  pub fn clear(&mut self) {
    self.lines.clear();
  }

  pub fn text(&self) -> String {
    let mut text = self.lines.iter().map(String::as_str).collect::<Vec<_>>().join("\r\n");
    if !text.is_empty() {
      text.push_str("\r\n");
    }
    text
  }
}

pub struct LogWindow {
  pub s: Surface,
  pub view: HWND,
  font: HFONT,
}

impl LogWindow {
  pub fn new(hwnd: HWND, theme: Theme, buffer: &LogBuffer) -> Self {
    let s = Surface::new(hwnd, theme);
    let style = WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32);
    let view = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        style,
        0,
        0,
        10,
        10,
        Some(hwnd),
        None,
        None,
        None,
      )
    }
    .expect("create log view");
    unsafe {
      let _ = SetWindowTheme(
        view,
        if theme.dark {
          w!("DarkMode_Explorer")
        } else {
          w!("Explorer")
        },
        PCWSTR::null(),
      );
    }
    let mut window = Self {
      s,
      view,
      font: HFONT::default(),
    };
    window.layout();
    window.set_text(&buffer.text());
    window
  }

  fn set_font(&mut self) {
    unsafe {
      if !self.font.is_invalid() {
        let _ = DeleteObject(self.font.into());
      }
      self.font = CreateFontW(
        -((13.0 * self.s.scale).round() as i32),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        FF_DONTCARE.0 as u32,
        w!("Cascadia Mono"),
      );
      SendMessageW(
        self.view,
        WM_SETFONT,
        Some(WPARAM(self.font.0 as usize)),
        Some(LPARAM(1)),
      );
    }
  }

  pub fn layout(&mut self) {
    self.set_font();
    let (w, h) = self.s.client_dips();
    self.s.hide_all();
    self.s.put(
      ID_COPY,
      rect(16.0, 10.0, 96.0, 32.0),
      Kind::Button {
        label: "Copy log".into(),
        style: ButtonStyle::Ghost,
      },
      true,
    );
    self.s.put(
      ID_CLEAR,
      rect(120.0, 10.0, 96.0, 32.0),
      Kind::Button {
        label: "Clear log".into(),
        style: ButtonStyle::Ghost,
      },
      true,
    );
    let pad = 12.0;
    let (x, y) = (self.s.px(16.0 + pad), self.s.px(TOOLBAR + pad));
    let (cw, ch) = (
      self.s.px((w - 32.0 - 2.0 * pad).max(10.0)),
      self.s.px((h - TOOLBAR - 16.0 - 2.0 * pad).max(10.0)),
    );
    unsafe {
      let _ = SetWindowPos(self.view, None, x, y, cw, ch, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    self.s.invalidate();
  }

  pub fn set_text(&self, text: &str) {
    unsafe {
      let _ = SetWindowTextW(self.view, &HSTRING::from(text));
    }
    self.scroll_to_end();
  }

  pub fn append(&self, line: &str) {
    unsafe {
      let len = GetWindowTextLengthW(self.view);
      // Drop the oldest half when the control grows too large.
      if len > 400_000 {
        return;
      }
      SendMessageW(
        self.view,
        0x00B1, /* EM_SETSEL */
        Some(WPARAM(len as usize)),
        Some(LPARAM(len as isize)),
      );
      let text = HSTRING::from(format!("{line}\r\n"));
      SendMessageW(
        self.view,
        0x00C2, /* EM_REPLACESEL */
        Some(WPARAM(0)),
        Some(LPARAM(text.as_ptr() as isize)),
      );
    }
    self.scroll_to_end();
  }

  fn scroll_to_end(&self) {
    unsafe {
      SendMessageW(self.view, WM_VSCROLL, Some(WPARAM(7 /* SB_BOTTOM */)), Some(LPARAM(0)));
    }
  }

  pub fn on_click(&mut self, id: Id) -> Vec<Effect> {
    match id {
      ID_COPY => vec![Effect::CopyLog],
      ID_CLEAR => vec![Effect::ClearLog],
      _ => vec![],
    }
  }

  pub fn paint(&self, c: &Canvas) {
    let t = &self.s.theme;
    c.clear(t.bg);
    let (w, h) = self.s.client_dips();
    let frame = rect(16.0, TOOLBAR, w - 32.0, h - TOOLBAR - 16.0);
    c.fill_round(frame, 10.0, t.surface.d2d());
    c.stroke_round(frame, 10.0, t.border.d2d(), 1.0);
    self.s.paint_nodes(c);
  }
}

impl Drop for LogWindow {
  fn drop(&mut self) {
    if !self.font.is_invalid() {
      unsafe {
        let _ = DeleteObject(self.font.into());
      }
    }
  }
}
