//! The main window: status shield, sign-in form or connection details, the
//! primary action and transient toasts, plus the Settings page shown in place.

use std::time::{Duration, Instant};

use gpdesktop::{
  config::Settings,
  controller::{ActionKind, Command, FormInput, StatusView, format_elapsed, now_unix},
  state::{StatusIcon, VpnStatus},
};
use windows::Win32::{Foundation::HWND, Graphics::Direct2D::Common::D2D_RECT_F};

use super::{
  Effect,
  gfx::{Align, Canvas, Font, Svg, measure, rect},
  prefs::{self, PrefsPanel},
  scene::{ButtonStyle, Glyph, Id, Kind, Surface},
  theme::Theme,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
  Home,
  Settings,
}

pub const WIDTH: f32 = 380.0;
pub const HEIGHT: f32 = 612.0;

const ID_MORE: Id = 1;
const ID_MODE: Id = 2;
const ID_PORTAL: Id = 3;
const ID_PORTAL_MENU: Id = 4;
const ID_GATEWAY: Id = 5;
const ID_USER: Id = 6;
const ID_PASS: Id = 7;
const ID_REMEMBER: Id = 8;
const ID_ACTION: Id = 9;

const MARGIN: f32 = 24.0;
const FIELD_H: f32 = 40.0;
const TOAST_FOR: Duration = Duration::from_secs(5);

pub struct MainWindow {
  pub s: Surface,
  pub view: StatusView,
  pub settings: Settings,
  e_portal: usize,
  e_gateway: usize,
  e_user: usize,
  e_pass: usize,
  remember: bool,
  toast: Option<(String, Instant)>,
  pub page: Page,
  pub prefs: PrefsPanel,
}

impl MainWindow {
  pub fn new(hwnd: HWND, theme: Theme, settings: Settings, saved_password: Option<String>) -> Self {
    let mut s = Surface::new(hwnd, theme);
    let e_portal = s.add_edit(ID_PORTAL, "vpn.example.com", false, false);
    let e_gateway = s.add_edit(ID_GATEWAY, "Gateway (optional)", false, false);
    let e_user = s.add_edit(ID_USER, "Username", false, false);
    let e_pass = s.add_edit(ID_PASS, "Password (empty for SSO)", true, false);
    let profile = settings.profile.clone();
    s.set_edit_text(e_portal, &profile.portal);
    s.set_edit_text(e_gateway, &profile.gateway);
    s.set_edit_text(e_user, &profile.username);
    if let Some(password) = saved_password {
      s.set_edit_text(e_pass, &password);
    }
    let prefs = PrefsPanel::new(&mut s);
    let view = StatusView {
      status: VpnStatus::Disconnected,
      icon: StatusIcon::Disconnected,
      title: VpnStatus::Disconnected.label().to_owned(),
      detail: "Not connected".to_owned(),
      failed: false,
      portal: profile.portal.clone(),
      gateway: String::new(),
      connected_at: None,
      form_locked: false,
      action: ActionKind::Connect,
      action_enabled: true,
    };
    let mut window = Self {
      s,
      view,
      settings,
      e_portal,
      e_gateway,
      e_user,
      e_pass,
      remember: profile.remember_password,
      toast: None,
      page: Page::Home,
      prefs,
    };
    window.layout();
    window
  }

  pub fn form(&self) -> FormInput {
    FormInput {
      portal: self.s.edit_text(self.e_portal).trim().to_owned(),
      gateway: if self.settings.as_gateway {
        String::new()
      } else {
        self.s.edit_text(self.e_gateway).trim().to_owned()
      },
      username: self.s.edit_text(self.e_user).trim().to_owned(),
      password: self.s.edit_text(self.e_pass),
      remember_password: self.remember,
    }
  }

  pub fn set_portal(&mut self, portal: &str) {
    self.s.set_edit_text(self.e_portal, portal);
  }

  pub fn set_status(&mut self, view: StatusView) {
    self.view = view;
    self.layout();
    self.s.invalidate();
  }

  pub fn set_settings(&mut self, settings: Settings) {
    self.settings = settings;
    self.layout();
    self.s.invalidate();
  }

  pub fn toast(&mut self, message: String) {
    self.toast = Some((message, Instant::now()));
    self.s.invalidate();
  }

  /// Once a second: refresh "Active for" and expire toasts.
  pub fn tick(&mut self) {
    if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() > TOAST_FOR) {
      self.toast = None;
      self.s.invalidate();
    }
    if self.view.connected_at.is_some() {
      self.s.invalidate();
    }
  }

  pub fn connected_view(&self) -> bool {
    matches!(self.view.status, VpnStatus::Connected | VpnStatus::Disconnecting)
  }

  /// Shows the Settings page with `settings` (Connection tab).
  pub fn open_settings(&mut self, settings: Settings) {
    self.prefs.open(&self.s, settings);
    self.page = Page::Settings;
    self.layout();
    self.s.invalidate();
  }

  pub fn close_settings(&mut self) {
    self.page = Page::Home;
    self.layout();
    self.s.invalidate();
  }

  // ---- layout -----------------------------------------------------------------------

  pub fn layout(&mut self) {
    if self.page == Page::Settings {
      self.s.hide_all();
      for i in [self.e_portal, self.e_gateway, self.e_user, self.e_pass] {
        self.s.hide_edit(i);
      }
      self.prefs.layout(&mut self.s, WIDTH, HEIGHT);
      return;
    }
    self.prefs.hide(&mut self.s);
    let connected = self.connected_view();
    let s = &mut self.s;
    s.hide_all();
    s.put(
      ID_MORE,
      rect(12.0, 12.0, 34.0, 30.0),
      Kind::IconButton { glyph: Glyph::More },
      true,
    );
    let inner = WIDTH - 2.0 * MARGIN;
    let locked = self.view.form_locked;
    let mut y;

    if connected {
      for i in [self.e_portal, self.e_gateway, self.e_user, self.e_pass] {
        s.hide_edit(i);
      }
      // Pill (232..264), details card (280..380).
      y = 404.0;
    } else {
      y = 246.0;
      // "Server address" row with the Portal/Gateway selector on the right.
      let mode = if self.settings.as_gateway { "Gateway" } else { "Portal" };
      s.put(
        ID_MODE,
        rect(WIDTH - MARGIN - 110.0, y - 2.0, 110.0, 24.0),
        Kind::Link {
          label: format!("{mode}  ▾"),
        },
        !locked,
      );
      y += 28.0;
      let recent = !self.settings.recent_portals.is_empty();
      s.place_field(
        ID_PORTAL,
        self.e_portal,
        rect(MARGIN, y, inner, FIELD_H),
        recent,
        !locked,
      );
      if recent {
        s.put(
          ID_PORTAL_MENU,
          rect(WIDTH - MARGIN - 34.0, y + 1.0, 33.0, FIELD_H - 2.0),
          Kind::IconButton { glyph: Glyph::Chevron },
          !locked,
        );
      }
      y += FIELD_H + 10.0;
      if self.settings.as_gateway {
        s.hide_edit(self.e_gateway);
      } else {
        s.place_field(
          ID_GATEWAY,
          self.e_gateway,
          rect(MARGIN, y, inner, FIELD_H),
          false,
          !locked,
        );
        y += FIELD_H + 10.0;
      }
      s.place_field(ID_USER, self.e_user, rect(MARGIN, y, inner, FIELD_H), false, !locked);
      y += FIELD_H + 10.0;
      s.place_field(ID_PASS, self.e_pass, rect(MARGIN, y, inner, FIELD_H), false, !locked);
      y += FIELD_H + 12.0;
      s.put(
        ID_REMEMBER,
        rect(MARGIN, y, inner, 22.0),
        Kind::Check {
          label: "Remember password".into(),
          on: self.remember,
        },
        !locked,
      );
      y += 22.0 + 20.0;
    }

    let style = match self.view.action {
      ActionKind::Connect => ButtonStyle::Primary,
      ActionKind::Cancel => ButtonStyle::Secondary,
      ActionKind::Disconnect => ButtonStyle::Secondary,
    };
    s.put(
      ID_ACTION,
      rect(MARGIN, y, inner, 44.0),
      Kind::Button {
        label: self.view.action.label().into(),
        style,
      },
      self.view.action_enabled,
    );
  }

  // ---- input -------------------------------------------------------------------------

  pub fn on_click(&mut self, id: Id) -> Vec<Effect> {
    if self.page == Page::Settings {
      if !prefs::owns(id) {
        return vec![];
      }
      let (effects, relayout) = self.prefs.on_click(&self.s, id);
      if relayout {
        self.layout();
        self.s.invalidate();
      }
      return effects;
    }
    match id {
      ID_MORE => vec![Effect::MainMenu],
      ID_MODE => vec![Effect::ModeMenu],
      ID_PORTAL_MENU => vec![Effect::PortalMenu],
      ID_REMEMBER => {
        self.remember = !self.remember;
        self.layout();
        self.s.invalidate();
        vec![]
      }
      ID_ACTION => self.primary_action(),
      _ => vec![],
    }
  }

  /// Enter key: Connect/Disconnect on the home page, Save on the Settings page.
  pub fn on_enter(&mut self) -> Vec<Effect> {
    match self.page {
      Page::Settings => self.prefs.save(&self.s),
      Page::Home => self.primary_action(),
    }
  }

  /// Esc key: leave the Settings page.
  pub fn on_escape(&mut self) -> Vec<Effect> {
    match self.page {
      Page::Settings => vec![Effect::ClosePrefs],
      Page::Home => vec![],
    }
  }

  /// The main button (also used by the tray menu).
  pub fn primary_action(&mut self) -> Vec<Effect> {
    if !self.view.action_enabled {
      return vec![];
    }
    match self.view.action {
      ActionKind::Connect => vec![Effect::Send(Command::Connect(self.form()))],
      ActionKind::Cancel | ActionKind::Disconnect => vec![Effect::Send(Command::Disconnect)],
    }
  }

  /// Anchor (client DIPs) for a menu opened by node `id`.
  pub fn menu_anchor(&self, effect: &Effect) -> (f32, f32) {
    let id = match effect {
      Effect::MainMenu => ID_MORE,
      Effect::ModeMenu => ID_MODE,
      Effect::PortalMenu => ID_PORTAL,
      _ => ID_MORE,
    };
    self
      .s
      .node(id)
      .map_or((0.0, 0.0), |n| (n.rect.left, n.rect.bottom + 4.0))
  }

  // ---- painting ------------------------------------------------------------------------

  pub fn paint(&self, c: &Canvas) {
    let t = &self.s.theme;
    c.clear(t.bg);
    if self.page == Page::Settings {
      self.prefs.paint(c, &self.s, WIDTH, HEIGHT);
      self.s.paint_nodes(c);
      self.paint_toast(c);
      return;
    }

    // Status shield.
    let (cx, cy, radius) = (WIDTH / 2.0, 112.0, 56.0);
    let (soft, ring) = match self.view.icon {
      StatusIcon::Connected => (t.success_soft, t.success),
      StatusIcon::Connecting => (t.warning_soft, t.warning),
      StatusIcon::Error => (t.danger_soft, t.danger),
      StatusIcon::Disconnected => (t.neutral_soft, t.border_strong),
    };
    c.fill_circle(cx, cy, radius, soft.d2d());
    c.stroke_circle(cx, cy, radius - 1.0, ring.alpha(0.55), 2.0);
    let svg = match self.view.icon {
      StatusIcon::Connected => Svg::Connected,
      StatusIcon::Connecting => Svg::Connecting,
      StatusIcon::Error => Svg::Error,
      StatusIcon::Disconnected => Svg::Disconnected,
    };
    c.svg(svg, rect(cx - 36.0, cy - 36.0, 72.0, 72.0), self.s.scale);

    // Title and detail.
    c.text(
      &self.view.title,
      rect(MARGIN, 178.0, WIDTH - 2.0 * MARGIN, 32.0),
      Font::strong(24.0),
      t.text.d2d(),
      Align::Center,
      true,
      false,
    );
    if !self.connected_view() {
      let color = if self.view.failed { t.danger } else { t.text_muted };
      c.text(
        &self.view.detail,
        rect(MARGIN, 212.0, WIDTH - 2.0 * MARGIN, 34.0),
        Font::body(13.0),
        color.d2d(),
        Align::Center,
        false,
        true,
      );
      c.text(
        "Server address",
        rect(MARGIN, 244.0, 160.0, 24.0),
        Font::body(13.0),
        t.text_muted.d2d(),
        Align::Leading,
        true,
        false,
      );
    } else {
      self.paint_connected(c);
    }

    self.s.paint_nodes(c);
    self.paint_toast(c);
  }

  fn paint_toast(&self, c: &Canvas) {
    let t = &self.s.theme;
    if let Some((message, _)) = &self.toast {
      let width = WIDTH - 2.0 * 16.0;
      let (_, h) = measure(message, Font::body(13.0), width - 28.0, true);
      let h = (h + 20.0).max(40.0);
      let r = rect(16.0, HEIGHT - 16.0 - h, width, h);
      c.fill_round(r, 10.0, t.toast_bg.alpha(0.96));
      c.text(
        message,
        rect(r.left + 14.0, r.top, width - 28.0, h),
        Font::body(13.0),
        t.toast_text.d2d(),
        Align::Leading,
        true,
        true,
      );
    }
  }

  fn paint_connected(&self, c: &Canvas) {
    let t = &self.s.theme;
    // "● Active for 00:04:20" pill.
    let pill_text = match self.view.connected_at {
      Some(since) if self.view.status == VpnStatus::Connected => {
        format!("Active for {}", format_elapsed(since, now_unix()))
      }
      _ => self.view.detail.clone(),
    };
    let (tw, _) = measure(&pill_text, Font::body(14.0), 400.0, false);
    let pw = tw + 44.0;
    let pill = rect((WIDTH - pw) / 2.0, 218.0, pw, 32.0);
    c.fill_round(pill, 16.0, t.surface.d2d());
    c.stroke_round(pill, 16.0, t.border.d2d(), 1.0);
    let dot = if self.view.status == VpnStatus::Connected {
      t.success
    } else {
      t.warning
    };
    c.fill_circle(pill.left + 18.0, pill.top + 16.0, 4.5, dot.d2d());
    c.text(
      &pill_text,
      D2D_RECT_F {
        left: pill.left + 30.0,
        right: pill.right - 10.0,
        ..pill
      },
      Font::body(14.0),
      t.text.d2d(),
      Align::Leading,
      true,
      false,
    );

    // Details card.
    let card = rect(MARGIN, 272.0, WIDTH - 2.0 * MARGIN, 112.0);
    c.fill_round(card, 12.0, t.surface.d2d());
    c.stroke_round(card, 12.0, t.border.d2d(), 1.0);
    let rows = [
      (
        "Gateway",
        if self.view.gateway.is_empty() {
          "—".to_owned()
        } else {
          self.view.gateway.clone()
        },
      ),
      ("Portal", self.view.portal.clone()),
    ];
    for (i, (label, value)) in rows.iter().enumerate() {
      let top = card.top + 4.0 + i as f32 * 52.0;
      let row = rect(card.left + 18.0, top, card.right - card.left - 36.0, 52.0);
      c.text(
        label,
        row,
        Font::body(14.0),
        t.text_muted.d2d(),
        Align::Leading,
        true,
        false,
      );
      c.text(
        value,
        D2D_RECT_F {
          left: row.left + 90.0,
          ..row
        },
        Font::strong(14.0),
        t.text.d2d(),
        Align::Trailing,
        true,
        false,
      );
      if i == 0 {
        c.line(row.left, top + 52.0, row.right, top + 52.0, t.border.d2d(), 1.0);
      }
    }
  }
}
