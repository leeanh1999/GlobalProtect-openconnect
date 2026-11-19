//! Preferences page shown inside the main window: the same options as
//! gpgui-gtk, grouped in tabs. It draws into the main window's `Surface` and
//! owns a few of its native EDITs.

use gpdesktop::config::{ClientOs, Settings};

use super::{
  Effect,
  gfx::{Align, Canvas, Font, rect},
  scene::{ButtonStyle, Glyph, Id, Kind, Surface},
};

const MARGIN: f32 = 24.0;
const ROW: f32 = 66.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
  Connection,
  Tunnel,
  Advanced,
  Application,
}

const TABS: [(Tab, &str); 4] = [
  (Tab::Connection, "Connection"),
  (Tab::Tunnel, "Tunnel"),
  (Tab::Advanced, "Advanced"),
  (Tab::Application, "Application"),
];

// Node ids (the main page uses 1..=9).
const ID_TAB: Id = 100; // ..104
const ID_BACK: Id = 50;
const ID_SAVE: Id = 51;
const ID_CANCEL: Id = 52;
const ID_OS: Id = 53;
// Toggles.
const T_AS_GATEWAY: Id = 20;
const T_AUTO_GATEWAY: Id = 21;
const T_BROWSER: Id = 22;
const T_AUTOSTART: Id = 23;
const T_NO_DTLS: Id = 24;
const T_IPV4: Id = 25;
const T_IGNORE_TLS: Id = 26;
const T_CLOSE_TRAY: Id = 27;
const T_START_MIN: Id = 28;
const T_RECONNECT: Id = 29;
// Edits.
const E_MTU: Id = 40;
const E_TIMEOUT: Id = 41;
const E_VERSION: Id = 42;
const E_DELAY: Id = 43;
const E_MAX: Id = 44;

/// Whether a node id belongs to this page.
pub fn owns(id: Id) -> bool {
  (20..=53).contains(&id) || (ID_TAB..ID_TAB + TABS.len() as Id).contains(&id)
}

pub struct PrefsPanel {
  draft: Settings,
  /// "Start with Windows" lives in the registry Run key, not in settings.json.
  autostart: bool,
  tab: Tab,
  e_mtu: usize,
  e_timeout: usize,
  e_version: usize,
  e_delay: usize,
  e_max: usize,
  /// Labels of edit/select rows from the last layout: (y, label, description).
  labels: Vec<(f32, &'static str, &'static str)>,
}

impl PrefsPanel {
  pub fn new(s: &mut Surface) -> Self {
    let panel = Self {
      draft: Settings::default(),
      autostart: false,
      tab: Tab::Connection,
      e_mtu: s.add_edit(E_MTU, "0", false, true),
      e_timeout: s.add_edit(E_TIMEOUT, "300", false, true),
      e_version: s.add_edit(E_VERSION, "e.g. 6.2.4-49", false, false),
      e_delay: s.add_edit(E_DELAY, "5", false, true),
      e_max: s.add_edit(E_MAX, "0", false, true),
      labels: Vec::new(),
    };
    panel.hide(s);
    panel
  }

  /// Starts editing `settings` (Connection tab first).
  pub fn open(&mut self, s: &Surface, settings: Settings) {
    s.set_edit_text(self.e_mtu, &settings.mtu.to_string());
    s.set_edit_text(self.e_timeout, &settings.reconnect_timeout.to_string());
    s.set_edit_text(self.e_version, &settings.client_version);
    s.set_edit_text(self.e_delay, &settings.reconnect_delay.to_string());
    s.set_edit_text(self.e_max, &settings.reconnect_max_attempts.to_string());
    self.draft = settings;
    self.autostart = super::autostart::is_enabled();
    self.tab = Tab::Connection;
  }

  pub fn hide(&self, s: &mut Surface) {
    for i in [self.e_mtu, self.e_timeout, self.e_version, self.e_delay, self.e_max] {
      s.hide_edit(i);
    }
  }

  fn toggle_value(&self, id: Id) -> bool {
    let d = &self.draft;
    match id {
      T_AS_GATEWAY => d.as_gateway,
      T_AUTO_GATEWAY => d.auto_gateway,
      T_BROWSER => d.use_default_browser,
      T_AUTOSTART => self.autostart,
      T_NO_DTLS => d.no_dtls,
      T_IPV4 => d.disable_ipv6,
      T_IGNORE_TLS => d.ignore_tls_errors,
      T_CLOSE_TRAY => d.close_to_tray,
      T_START_MIN => d.start_minimized,
      T_RECONNECT => d.auto_reconnect,
      _ => false,
    }
  }

  fn flip(&mut self, id: Id) {
    let d = &mut self.draft;
    let slot = match id {
      T_AS_GATEWAY => &mut d.as_gateway,
      T_AUTO_GATEWAY => &mut d.auto_gateway,
      T_BROWSER => &mut d.use_default_browser,
      T_AUTOSTART => &mut self.autostart,
      T_NO_DTLS => &mut d.no_dtls,
      T_IPV4 => &mut d.disable_ipv6,
      T_IGNORE_TLS => &mut d.ignore_tls_errors,
      T_CLOSE_TRAY => &mut d.close_to_tray,
      T_START_MIN => &mut d.start_minimized,
      T_RECONNECT => &mut d.auto_reconnect,
      _ => return,
    };
    *slot = !*slot;
  }

  /// Lays the page out in a `width`×`height` DIP window (the caller has hidden other nodes).
  pub fn layout(&mut self, s: &mut Surface, width: f32, height: f32) {
    self.hide(s);
    self.labels.clear();
    let inner = width - 2.0 * MARGIN;
    s.put(
      ID_BACK,
      rect(12.0, 12.0, 34.0, 30.0),
      Kind::IconButton { glyph: Glyph::Back },
      true,
    );

    // Segmented tabs.
    let tab_w = (inner - 8.0) / TABS.len() as f32;
    for (i, (tab, label)) in TABS.iter().enumerate() {
      s.put(
        ID_TAB + i as Id,
        rect(MARGIN + 4.0 + i as f32 * tab_w, 60.0, tab_w, 30.0),
        Kind::Tab {
          label: (*label).into(),
          selected: *tab == self.tab,
        },
        true,
      );
    }

    let mut y = 112.0;
    let toggle = |this: &mut Self, s: &mut Surface, y: &mut f32, id: Id, label: &str, desc: &str| {
      let on = this.toggle_value(id);
      s.put(
        id,
        rect(MARGIN, *y, inner, 52.0),
        Kind::Toggle {
          label: label.into(),
          desc: desc.into(),
          on,
        },
        true,
      );
      *y += ROW;
    };
    let field = |this: &mut Self,
                 s: &mut Surface,
                 y: &mut f32,
                 id: Id,
                 index: usize,
                 w: f32,
                 label: &'static str,
                 desc: &'static str| {
      s.place_field(id, index, rect(width - MARGIN - w, *y, w, 36.0), false, true);
      this.labels.push((*y, label, desc));
      *y += ROW;
    };
    match self.tab {
      Tab::Connection => {
        toggle(
          self,
          s,
          &mut y,
          T_AS_GATEWAY,
          "Connect as gateway",
          "Treat the address as a gateway, not a portal",
        );
        toggle(
          self,
          s,
          &mut y,
          T_AUTO_GATEWAY,
          "Automatic gateway",
          "Try gateways by priority when none is given",
        );
        toggle(
          self,
          s,
          &mut y,
          T_BROWSER,
          "Use default browser for SSO",
          "Sign in with SAML in the system browser instead of the embedded window",
        );
      }
      Tab::Tunnel => {
        toggle(self, s, &mut y, T_NO_DTLS, "Use TLS only", "Disable DTLS and ESP");
        toggle(
          self,
          s,
          &mut y,
          T_IPV4,
          "IPv4 only",
          "Do not route IPv6 traffic through the tunnel",
        );
        field(self, s, &mut y, E_MTU, self.e_mtu, 90.0, "MTU", "0 = automatic");
        field(
          self,
          s,
          &mut y,
          E_TIMEOUT,
          self.e_timeout,
          90.0,
          "Tunnel retry timeout",
          "Seconds the tunnel keeps retrying a dropped connection",
        );
      }
      Tab::Advanced => {
        let os = self.draft.os.as_arg().to_owned();
        s.put(
          ID_OS,
          rect(width - MARGIN - 130.0, y, 130.0, 36.0),
          Kind::Select { value: os },
          true,
        );
        self
          .labels
          .push((y, "Reported OS", "Client OS sent to the portal and gateway"));
        y += ROW;
        field(
          self,
          s,
          &mut y,
          E_VERSION,
          self.e_version,
          130.0,
          "Client version",
          "Optional GlobalProtect version to report",
        );
        toggle(
          self,
          s,
          &mut y,
          T_IGNORE_TLS,
          "Ignore TLS errors",
          "Insecure: accept invalid server certificates",
        );
      }
      Tab::Application => {
        toggle(
          self,
          s,
          &mut y,
          T_AUTOSTART,
          "Start with Windows",
          "Open in the system tray when you sign in to Windows",
        );
        toggle(
          self,
          s,
          &mut y,
          T_CLOSE_TRAY,
          "Close to tray",
          "Keep running in the system tray when the window is closed",
        );
        toggle(
          self,
          s,
          &mut y,
          T_START_MIN,
          "Start minimized",
          "Start hidden in the system tray",
        );
        toggle(
          self,
          s,
          &mut y,
          T_RECONNECT,
          "Reconnect automatically",
          "Start a new connection when an established one drops",
        );
        field(
          self,
          s,
          &mut y,
          E_DELAY,
          self.e_delay,
          90.0,
          "First retry after",
          "Seconds; doubles after each failed attempt, up to 5 minutes",
        );
        field(
          self,
          s,
          &mut y,
          E_MAX,
          self.e_max,
          90.0,
          "Maximum attempts",
          "Give up after this many failures in a row (0 = keep trying)",
        );
      }
    }

    let bw = (inner - 10.0) / 2.0;
    let by = height - 24.0 - 40.0;
    s.put(
      ID_CANCEL,
      rect(MARGIN, by, bw, 40.0),
      Kind::Button {
        label: "Cancel".into(),
        style: ButtonStyle::Secondary,
      },
      true,
    );
    s.put(
      ID_SAVE,
      rect(MARGIN + bw + 10.0, by, bw, 40.0),
      Kind::Button {
        label: "Save".into(),
        style: ButtonStyle::Primary,
      },
      true,
    );
  }

  fn collect(&mut self, s: &Surface) {
    let num = |i: usize, old: u32| s.edit_text(i).trim().parse::<u32>().unwrap_or(old);
    self.draft.mtu = num(self.e_mtu, self.draft.mtu);
    self.draft.reconnect_timeout = num(self.e_timeout, self.draft.reconnect_timeout);
    self.draft.reconnect_delay = num(self.e_delay, self.draft.reconnect_delay).max(1);
    self.draft.reconnect_max_attempts = num(self.e_max, self.draft.reconnect_max_attempts);
    self.draft.client_version = s.edit_text(self.e_version).trim().to_owned();
  }

  /// Enter key: save.
  pub fn save(&mut self, s: &Surface) -> Vec<Effect> {
    self.collect(s);
    // HIP is not offered on Windows (no posture helper).
    self.draft.hip = false;
    vec![Effect::SavePrefs(Box::new(self.draft.clone()), self.autostart)]
  }

  /// Returns effects, and whether the page must be laid out again.
  pub fn on_click(&mut self, s: &Surface, id: Id) -> (Vec<Effect>, bool) {
    match id {
      ID_SAVE => (self.save(s), false),
      ID_CANCEL | ID_BACK => (vec![Effect::ClosePrefs], false),
      ID_OS => (vec![Effect::OsMenu], false),
      id if (ID_TAB..ID_TAB + TABS.len() as Id).contains(&id) => {
        self.tab = TABS[(id - ID_TAB) as usize].0;
        (vec![], true)
      }
      id => {
        self.flip(id);
        (vec![], true)
      }
    }
  }

  /// Screenshot mode: open the Application tab.
  pub fn show_application_tab(&mut self) {
    self.tab = Tab::Application;
  }

  pub fn set_os(&mut self, os: ClientOs) {
    self.draft.os = os;
  }

  pub fn os(&self) -> ClientOs {
    self.draft.os
  }

  pub fn os_anchor(&self, s: &Surface) -> (f32, f32) {
    s.node(ID_OS).map_or((0.0, 0.0), |n| (n.rect.left, n.rect.bottom + 4.0))
  }

  pub fn paint(&self, c: &Canvas, s: &Surface, width: f32, height: f32) {
    let t = &s.theme;
    c.text(
      "Settings",
      rect(MARGIN + 30.0, 12.0, width - 2.0 * MARGIN - 60.0, 30.0),
      Font::strong(16.0),
      t.text.d2d(),
      Align::Center,
      true,
      false,
    );
    // Tab strip background.
    c.fill_round(rect(MARGIN, 56.0, width - 2.0 * MARGIN, 38.0), 9.0, t.secondary.d2d());
    let label_w = width - 2.0 * MARGIN - 150.0;
    for (y, label, desc) in &self.labels {
      c.text(
        label,
        rect(MARGIN, *y, label_w, 20.0),
        Font::body(14.0),
        t.text.d2d(),
        Align::Leading,
        true,
        false,
      );
      c.text(
        desc,
        rect(MARGIN, y + 21.0, label_w, 34.0),
        Font::body(12.0),
        t.text_muted.d2d(),
        Align::Leading,
        false,
        true,
      );
    }
    c.line(
      MARGIN,
      height - 80.0,
      width - MARGIN,
      height - 80.0,
      t.border.d2d(),
      1.0,
    );
  }
}
