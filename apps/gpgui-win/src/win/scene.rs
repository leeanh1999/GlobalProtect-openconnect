//! A tiny retained widget set drawn with Direct2D. Text inputs are native
//! borderless EDIT controls placed inside custom-drawn frames, so IME,
//! selection, clipboard and password masking behave like any Windows app.

use windows::{
  Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    Graphics::{
      Direct2D::Common::D2D_RECT_F,
      Dwm::{
        DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
      },
      Gdi::{
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DeleteObject,
        FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS,
      },
    },
    UI::{
      Controls::SetWindowTheme,
      HiDpi::GetDpiForWindow,
      Input::KeyboardAndMouse::EnableWindow,
      WindowsAndMessaging::{
        CreateWindowExW, ES_AUTOHSCROLL, ES_PASSWORD, GetClientRect, GetWindowTextLengthW, GetWindowTextW, HMENU,
        SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetWindowPos, SetWindowTextW, ShowWindow,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT, WS_CHILD, WS_TABSTOP,
      },
    },
  },
  core::{HSTRING, PCWSTR, w},
};

use super::{
  gfx::{Align, Canvas, Font, contains, inset, rect},
  theme::Theme,
};

pub type Id = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
  Primary,
  Secondary,
  Danger,
  /// Text-only, used for small header actions.
  Ghost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
  More,
  Chevron,
  Back,
}

#[derive(Debug, Clone)]
pub enum Kind {
  Button {
    label: String,
    style: ButtonStyle,
  },
  IconButton {
    glyph: Glyph,
  },
  /// On/off switch with a label and optional description underneath.
  Toggle {
    label: String,
    desc: String,
    on: bool,
  },
  Check {
    label: String,
    on: bool,
  },
  /// Frame around a native EDIT (`edit` index into `Surface::edits`).
  Field {
    edit: usize,
    chevron: bool,
  },
  /// A value that opens a menu when clicked.
  Select {
    value: String,
  },
  /// Small accent-coloured clickable text.
  Link {
    label: String,
  },
  /// Segmented tab.
  Tab {
    label: String,
    selected: bool,
  },
}

#[derive(Debug, Clone)]
pub struct Node {
  pub id: Id,
  pub rect: D2D_RECT_F,
  pub kind: Kind,
  pub enabled: bool,
  pub visible: bool,
}

/// A native single-line text box.
pub struct Edit {
  pub hwnd: HWND,
  /// Where the frame is (DIPs); the EDIT itself sits inside it.
  pub frame: D2D_RECT_F,
  pub visible: bool,
  pub enabled: bool,
  /// Leaves room on the right for a chevron button.
  pub chevron: bool,
}

/// Common per-window state: geometry, theme, widgets and native children.
pub struct Surface {
  pub hwnd: HWND,
  pub scale: f32,
  pub theme: Theme,
  pub nodes: Vec<Node>,
  pub edits: Vec<Edit>,
  pub hover: Option<Id>,
  pub pressed: Option<Id>,
  pub tracking_mouse: bool,
  font: HFONT,
  font_scale: f32,
  edit_brush: HBRUSH,
  pub target: super::gfx::WindowTarget,
}

impl Surface {
  pub fn new(hwnd: HWND, theme: Theme) -> Self {
    let mut surface = Self {
      hwnd,
      scale: window_scale(hwnd),
      theme,
      nodes: Vec::new(),
      edits: Vec::new(),
      hover: None,
      pressed: None,
      tracking_mouse: false,
      font: HFONT::default(),
      font_scale: 0.0,
      edit_brush: HBRUSH::default(),
      target: super::gfx::WindowTarget::new(),
    };
    surface.apply_theme(theme);
    surface
  }

  pub fn apply_theme(&mut self, theme: Theme) {
    self.theme = theme;
    unsafe {
      let dark: i32 = theme.dark.into();
      let _ = DwmSetWindowAttribute(
        self.hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const _ as _,
        std::mem::size_of::<i32>() as u32,
      );
      // Round corners (Windows 11) and a title bar matching the window background.
      let round: i32 = 2;
      let _ = DwmSetWindowAttribute(
        self.hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &round as *const _ as _,
        std::mem::size_of::<i32>() as u32,
      );
      let caption = theme.bg.colorref();
      let _ = DwmSetWindowAttribute(
        self.hwnd,
        DWMWA_CAPTION_COLOR,
        &caption as *const _ as _,
        std::mem::size_of_val(&caption) as u32,
      );
      if !self.edit_brush.is_invalid() {
        let _ = DeleteObject(self.edit_brush.into());
      }
      self.edit_brush = CreateSolidBrush(theme.surface.colorref());
      for edit in &self.edits {
        let _ = SetWindowTheme(
          edit.hwnd,
          if theme.dark {
            w!("DarkMode_Explorer")
          } else {
            w!("Explorer")
          },
          PCWSTR::null(),
        );
      }
    }
    self.invalidate();
  }

  pub fn invalidate(&self) {
    unsafe {
      let _ = InvalidateRect(Some(self.hwnd), None, false);
    }
  }

  pub fn px(&self, dip: f32) -> i32 {
    (dip * self.scale).round() as i32
  }

  pub fn client_dips(&self) -> (f32, f32) {
    let mut rc = RECT::default();
    unsafe {
      let _ = GetClientRect(self.hwnd, &mut rc);
    }
    (
      (rc.right - rc.left) as f32 / self.scale,
      (rc.bottom - rc.top) as f32 / self.scale,
    )
  }

  // ---- nodes -------------------------------------------------------------------

  pub fn node(&self, id: Id) -> Option<&Node> {
    self.nodes.iter().find(|n| n.id == id)
  }

  pub fn node_mut(&mut self, id: Id) -> Option<&mut Node> {
    self.nodes.iter_mut().find(|n| n.id == id)
  }

  /// Inserts or updates a node.
  pub fn put(&mut self, id: Id, rect: D2D_RECT_F, kind: Kind, enabled: bool) {
    match self.node_mut(id) {
      Some(node) => {
        node.rect = rect;
        node.kind = kind;
        node.enabled = enabled;
        node.visible = true;
      }
      None => self.nodes.push(Node {
        id,
        rect,
        kind,
        enabled,
        visible: true,
      }),
    }
  }

  pub fn hide_all(&mut self) {
    for node in &mut self.nodes {
      node.visible = false;
    }
  }

  pub fn hit(&self, x: f32, y: f32) -> Option<Id> {
    self
      .nodes
      .iter()
      .rev()
      .find(|n| n.visible && n.enabled && contains(&n.rect, x, y))
      .map(|n| n.id)
  }

  // ---- native edits ------------------------------------------------------------------

  fn edit_font(&mut self) -> HFONT {
    if self.font_scale != self.scale || self.font.is_invalid() {
      unsafe {
        if !self.font.is_invalid() {
          let _ = DeleteObject(self.font.into());
        }
        self.font = CreateFontW(
          -((14.0 * self.scale).round() as i32),
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
          w!("Segoe UI"),
        );
      }
      self.font_scale = self.scale;
      for edit in &self.edits {
        unsafe {
          SendMessageW(
            edit.hwnd,
            WM_SETFONT,
            Some(WPARAM(self.font.0 as usize)),
            Some(LPARAM(1)),
          );
        }
      }
    }
    self.font
  }

  /// Creates a native EDIT; returns its index in `edits`.
  pub fn add_edit(&mut self, id: Id, placeholder: &str, password: bool, numeric: bool) -> usize {
    let mut style = WS_CHILD | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
    if password {
      style |= WINDOW_STYLE(ES_PASSWORD as u32);
    }
    if numeric {
      style |= WINDOW_STYLE(windows::Win32::UI::WindowsAndMessaging::ES_NUMBER as u32);
    }
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        style,
        0,
        0,
        10,
        10,
        Some(self.hwnd),
        Some(HMENU(id as usize as *mut _)),
        None,
        None,
      )
    }
    .expect("create edit");
    let cue = HSTRING::from(placeholder);
    unsafe {
      // EM_SETCUEBANNER
      SendMessageW(hwnd, 0x1501, Some(WPARAM(0)), Some(LPARAM(cue.as_ptr() as isize)));
      let _ = SetWindowTheme(
        hwnd,
        if self.theme.dark {
          w!("DarkMode_Explorer")
        } else {
          w!("Explorer")
        },
        PCWSTR::null(),
      );
    }
    self.edits.push(Edit {
      hwnd,
      frame: rect(0.0, 0.0, 0.0, 0.0),
      visible: false,
      enabled: true,
      chevron: false,
    });
    let font = self.edit_font();
    unsafe {
      SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
    self.edits.len() - 1
  }

  /// Positions edit `index` inside a field frame and adds the frame node.
  pub fn place_field(&mut self, id: Id, index: usize, frame: D2D_RECT_F, chevron: bool, enabled: bool) {
    self.put(id, frame, Kind::Field { edit: index, chevron }, enabled);
    self.edit_font();
    let pad = 12.0;
    let right = if chevron { 34.0 } else { pad };
    let line = 20.0;
    let x = frame.left + pad;
    let y = frame.top + ((frame.bottom - frame.top) - line) / 2.0;
    let w = (frame.right - frame.left) - pad - right;
    let (px, py, pw, ph) = (self.px(x), self.px(y), self.px(w), self.px(line));
    let edit = &mut self.edits[index];
    edit.frame = frame;
    edit.visible = true;
    edit.enabled = enabled;
    edit.chevron = chevron;
    unsafe {
      let _ = SetWindowPos(edit.hwnd, None, px, py, pw, ph, SWP_NOZORDER | SWP_NOACTIVATE);
      let _ = ShowWindow(edit.hwnd, SW_SHOW);
      let _ = EnableWindow(edit.hwnd, enabled);
    }
  }

  pub fn hide_edit(&mut self, index: usize) {
    let edit = &mut self.edits[index];
    edit.visible = false;
    unsafe {
      let _ = ShowWindow(edit.hwnd, SW_HIDE);
    }
  }

  pub fn edit_text(&self, index: usize) -> String {
    let hwnd = self.edits[index].hwnd;
    unsafe {
      let len = GetWindowTextLengthW(hwnd);
      let mut buf = vec![0u16; len as usize + 1];
      let n = GetWindowTextW(hwnd, &mut buf);
      String::from_utf16_lossy(&buf[..n as usize])
    }
  }

  pub fn set_edit_text(&self, index: usize, text: &str) {
    unsafe {
      let _ = SetWindowTextW(self.edits[index].hwnd, &HSTRING::from(text));
    }
  }

  /// Brushes for WM_CTLCOLOREDIT / WM_CTLCOLORSTATIC of our edits.
  pub fn edit_colors(
    &self,
    enabled: bool,
  ) -> (
    windows::Win32::Foundation::COLORREF,
    windows::Win32::Foundation::COLORREF,
    HBRUSH,
  ) {
    let text = if enabled {
      self.theme.text
    } else {
      self.theme.text_muted
    };
    (text.colorref(), self.theme.surface.colorref(), self.edit_brush)
  }

  /// Re-reads the window DPI; returns true if it changed.
  pub fn refresh_scale(&mut self) -> bool {
    let scale = window_scale(self.hwnd);
    if (scale - self.scale).abs() > f32::EPSILON {
      self.scale = scale;
      self.target.discard();
      self.edit_font();
      true
    } else {
      false
    }
  }

  // ---- painting ----------------------------------------------------------------------

  pub fn paint_nodes(&self, c: &Canvas) {
    for node in self.nodes.iter().filter(|n| n.visible) {
      self.paint_node(c, node);
    }
  }

  fn paint_node(&self, c: &Canvas, node: &Node) {
    let t = &self.theme;
    let hover = self.hover == Some(node.id) && node.enabled;
    let pressed = hover && self.pressed == Some(node.id);
    let r = node.rect;
    let alpha = if node.enabled { 1.0 } else { 0.45 };
    match &node.kind {
      Kind::Button { label, style } => {
        let (bg, fg, border) = match style {
          ButtonStyle::Primary => (
            if pressed {
              t.accent_pressed
            } else if hover {
              t.accent_hover
            } else {
              t.accent
            },
            t.on_accent,
            None,
          ),
          ButtonStyle::Secondary => (
            if pressed {
              t.secondary_pressed
            } else if hover {
              t.secondary_hover
            } else {
              t.secondary
            },
            t.text,
            None,
          ),
          ButtonStyle::Danger => (
            if pressed {
              t.secondary_pressed
            } else if hover {
              t.secondary_hover
            } else {
              t.secondary
            },
            t.danger,
            None,
          ),
          ButtonStyle::Ghost => (
            if pressed {
              t.secondary_pressed
            } else if hover {
              t.secondary
            } else {
              t.bg
            },
            t.text,
            Some(t.border),
          ),
        };
        c.fill_round(r, 8.0, bg.alpha(alpha));
        if let Some(border) = border {
          c.stroke_round(r, 8.0, border.alpha(alpha), 1.0);
        }
        c.text(
          label,
          r,
          Font::strong(14.0),
          fg.alpha(alpha),
          Align::Center,
          true,
          false,
        );
      }
      Kind::IconButton { glyph } => {
        if hover {
          let bg = if pressed { t.secondary_pressed } else { t.secondary };
          c.fill_round(r, 6.0, bg.d2d());
        }
        draw_glyph(c, *glyph, r, t.text_muted.alpha(alpha));
      }
      Kind::Toggle { label, desc, on } => {
        let track_w = 40.0;
        let track_h = 20.0;
        let tx = r.right - track_w;
        let ty = r.top + 2.0;
        let text_r = D2D_RECT_F { right: tx - 16.0, ..r };
        c.text(
          label,
          rect(text_r.left, r.top, text_r.right - text_r.left, 20.0),
          Font::body(14.0),
          t.text.alpha(alpha),
          Align::Leading,
          true,
          false,
        );
        if !desc.is_empty() {
          c.text(
            desc,
            rect(
              text_r.left,
              r.top + 21.0,
              text_r.right - text_r.left,
              r.bottom - r.top - 21.0,
            ),
            Font::body(12.0),
            t.text_muted.alpha(alpha),
            Align::Leading,
            false,
            true,
          );
        }
        let track = rect(tx, ty, track_w, track_h);
        if *on {
          let bg = if hover { t.accent_hover } else { t.accent };
          c.fill_round(track, track_h / 2.0, bg.alpha(alpha));
          c.fill_circle(
            tx + track_w - track_h / 2.0,
            ty + track_h / 2.0,
            6.0,
            t.on_accent.alpha(alpha),
          );
        } else {
          let border = if hover { t.text } else { t.text_muted };
          c.fill_round(track, track_h / 2.0, t.surface.alpha(alpha));
          c.stroke_round(track, track_h / 2.0, border.alpha(alpha), 1.0);
          c.fill_circle(tx + track_h / 2.0, ty + track_h / 2.0, 5.0, border.alpha(alpha));
        }
      }
      Kind::Check { label, on } => {
        let size = 18.0;
        let bx = rect(r.left, r.top + ((r.bottom - r.top) - size) / 2.0, size, size);
        if *on {
          c.fill_round(bx, 4.0, t.accent.alpha(alpha));
          let (x, y) = (bx.left, bx.top);
          c.line(x + 4.5, y + 9.5, x + 7.5, y + 12.5, t.on_accent.alpha(alpha), 1.8);
          c.line(x + 7.5, y + 12.5, x + 13.5, y + 6.0, t.on_accent.alpha(alpha), 1.8);
        } else {
          c.fill_round(bx, 4.0, t.surface.alpha(alpha));
          let border = if hover { t.text } else { t.border_strong };
          c.stroke_round(bx, 4.0, border.alpha(alpha), 1.0);
        }
        c.text(
          label,
          D2D_RECT_F {
            left: r.left + size + 10.0,
            ..r
          },
          Font::body(14.0),
          t.text.alpha(alpha),
          Align::Leading,
          true,
          false,
        );
      }
      Kind::Field { edit, chevron } => {
        let focused = self
          .edits
          .get(*edit)
          .is_some_and(|e| unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } == e.hwnd);
        // Same colour as the EDIT inside it (WM_CTLCOLOR*), enabled or not.
        c.fill_round(r, 8.0, t.surface.d2d());
        let border = if focused {
          t.accent
        } else if hover {
          t.border_strong
        } else if node.enabled {
          t.border
        } else {
          t.secondary
        };
        c.stroke_round(r, 8.0, border.d2d(), if focused { 1.5 } else { 1.0 });
        if *chevron {
          let cr = rect(r.right - 34.0, r.top, 34.0, r.bottom - r.top);
          draw_glyph(c, Glyph::Chevron, cr, t.text_muted.alpha(alpha));
        }
      }
      Kind::Select { value } => {
        let bg = if hover { t.surface_hover } else { t.surface };
        c.fill_round(r, 8.0, bg.alpha(alpha));
        c.stroke_round(r, 8.0, t.border.alpha(alpha), 1.0);
        c.text(
          value,
          inset(
            &D2D_RECT_F {
              right: r.right - 28.0,
              ..r
            },
            12.0,
            0.0,
          ),
          Font::body(14.0),
          t.text.alpha(alpha),
          Align::Leading,
          true,
          false,
        );
        draw_glyph(
          c,
          Glyph::Chevron,
          rect(r.right - 32.0, r.top, 32.0, r.bottom - r.top),
          t.text_muted.alpha(alpha),
        );
      }
      Kind::Link { label } => {
        let color = if hover { t.accent_hover } else { t.accent };
        c.text(
          label,
          r,
          Font::strong(13.0),
          color.alpha(alpha),
          Align::Trailing,
          true,
          false,
        );
      }
      Kind::Tab { label, selected } => {
        if *selected {
          c.fill_round(r, 7.0, t.surface.d2d());
          c.stroke_round(r, 7.0, t.border.d2d(), 1.0);
        } else if hover {
          c.fill_round(r, 7.0, t.secondary.d2d());
        }
        let color = if *selected { t.text } else { t.text_muted };
        c.text(label, r, Font::strong(13.0), color.d2d(), Align::Center, true, false);
      }
    }
  }
}

impl Drop for Surface {
  fn drop(&mut self) {
    unsafe {
      if !self.font.is_invalid() {
        let _ = DeleteObject(self.font.into());
      }
      if !self.edit_brush.is_invalid() {
        let _ = DeleteObject(self.edit_brush.into());
      }
    }
  }
}

fn draw_glyph(
  c: &Canvas,
  glyph: Glyph,
  r: D2D_RECT_F,
  color: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
) {
  let cx = (r.left + r.right) / 2.0;
  let cy = (r.top + r.bottom) / 2.0;
  match glyph {
    Glyph::More => {
      for dx in [-6.0, 0.0, 6.0] {
        c.fill_circle(cx + dx, cy, 1.8, color);
      }
    }
    Glyph::Chevron => {
      c.line(cx - 5.0, cy - 2.5, cx, cy + 2.5, color, 1.6);
      c.line(cx, cy + 2.5, cx + 5.0, cy - 2.5, color, 1.6);
    }
    Glyph::Back => {
      c.line(cx + 2.5, cy - 6.0, cx - 3.5, cy, color, 1.8);
      c.line(cx - 3.5, cy, cx + 2.5, cy + 6.0, color, 1.8);
    }
  }
}

pub fn window_scale(hwnd: HWND) -> f32 {
  if let Some(scale) = std::env::var("GPGUI_WIN_SCALE")
    .ok()
    .and_then(|s| s.parse::<f32>().ok())
  {
    return scale.clamp(0.5, 4.0);
  }
  let dpi = unsafe { GetDpiForWindow(hwnd) };
  if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}
