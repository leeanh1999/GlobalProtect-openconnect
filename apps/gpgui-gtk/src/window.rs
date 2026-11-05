use std::{
  cell::{Cell, RefCell},
  rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
  config::Settings,
  icons, preferences, secrets,
  state::VpnStatus,
  tray::{TrayCommand, TrayHandle},
  vpn::{self, ConnectRequest, LogLevel, VpnEvent},
};

const MAX_LOG_LINES: i32 = 2000;

struct Widgets {
  window: adw::ApplicationWindow,
  toasts: adw::ToastOverlay,
  status_icon: gtk::Image,
  status_title: gtk::Label,
  status_detail: gtk::Label,
  form: adw::PreferencesGroup,
  portal: adw::EntryRow,
  recent_list: gtk::ListBox,
  recent_button: gtk::MenuButton,
  gateway: adw::EntryRow,
  username: adw::EntryRow,
  password: adw::PasswordEntryRow,
  remember: gtk::CheckButton,
  action: gtk::Button,
}

pub struct App {
  app: adw::Application,
  w: Widgets,
  settings: RefCell<Settings>,
  status: Cell<VpnStatus>,
  /// gpclient process is alive (from spawn until its Exited event).
  running: Cell<bool>,
  user_disconnect: Cell<bool>,
  quit_after_disconnect: Cell<bool>,
  /// gpclient asked for `--fix-openssl` during the current attempt.
  needs_fix_openssl: Cell<bool>,
  /// gpclient refused to start because another instance holds its lock.
  another_instance: Cell<bool>,
  /// pid of our own gpclient, never offered for killing.
  child_pid: Cell<Option<u32>>,
  last_error: RefCell<Option<String>>,
  /// Connection log, shown on demand from the menu.
  log: gtk::TextBuffer,
  log_view: RefCell<Option<glib::WeakRef<gtk::TextView>>>,
  /// The current gpclient run reached "Connected".
  was_connected: Cell<bool>,
  /// The current gpclient run is an automatic reconnect.
  reconnecting: Cell<bool>,
  /// Failed reconnect attempts in a row.
  reconnect_failures: Cell<u32>,
  reconnect_remaining: Cell<u32>,
  reconnect_timer: RefCell<Option<glib::SourceId>>,
  tray: TrayHandle,
  vpn_tx: async_channel::Sender<VpnEvent>,
  _hold: RefCell<Option<gio::ApplicationHoldGuard>>,
}

pub fn setup(app: &adw::Application) {
  let instance: Rc<RefCell<Option<Rc<App>>>> = Rc::default();
  app.connect_activate(move |app| {
    if let Some(existing) = instance.borrow().as_ref() {
      existing.show();
      return;
    }
    let created = App::new(app);
    let settings = created.settings.borrow().clone();
    if !(settings.start_minimized && created.tray.is_available()) {
      created.show();
    }
    created.maybe_screenshot();
    instance.replace(Some(created));
  });
}

impl App {
  fn new(app: &adw::Application) -> Rc<Self> {
    let settings = Settings::load();
    load_icons(icons::install().as_deref());
    let w = build_widgets(app, &settings);

    let (vpn_tx, vpn_rx) = async_channel::unbounded::<VpnEvent>();
    let (tray_tx, tray_rx) = async_channel::unbounded::<TrayCommand>();
    let tray = TrayHandle::spawn(tray_tx, icons::tray_pixmaps());

    let this = Rc::new(Self {
      app: app.clone(),
      w,
      settings: RefCell::new(settings),
      status: Cell::new(VpnStatus::Disconnected),
      running: Cell::new(false),
      user_disconnect: Cell::new(false),
      quit_after_disconnect: Cell::new(false),
      needs_fix_openssl: Cell::new(false),
      another_instance: Cell::new(false),
      child_pid: Cell::new(None),
      last_error: RefCell::default(),
      log: gtk::TextBuffer::new(None),
      log_view: RefCell::default(),
      was_connected: Cell::new(false),
      reconnecting: Cell::new(false),
      reconnect_failures: Cell::new(0),
      reconnect_remaining: Cell::new(0),
      reconnect_timer: RefCell::default(),
      tray,
      vpn_tx,
      _hold: RefCell::new(Some(app.hold())),
    });

    this.install_actions();
    this.connect_signals();
    this.refresh_recent();
    this.load_saved_password();
    this.set_status(VpnStatus::Disconnected);

    let weak = Rc::downgrade(&this);
    glib::spawn_future_local(async move {
      while let Ok(event) = vpn_rx.recv().await {
        let Some(this) = weak.upgrade() else { break };
        this.on_vpn_event(event);
      }
    });

    let weak = Rc::downgrade(&this);
    glib::spawn_future_local(async move {
      while let Ok(cmd) = tray_rx.recv().await {
        let Some(this) = weak.upgrade() else { break };
        match cmd {
          TrayCommand::ShowWindow => this.show(),
          TrayCommand::Connect => {
            this.show();
            this.connect();
          }
          TrayCommand::Disconnect => this.disconnect(),
          TrayCommand::Quit => this.request_quit(),
        }
      }
    });

    this
  }

  /// Dev aid: `GPGUI_GTK_SCREENSHOT=out.png [GPGUI_GTK_DEMO=state]` renders the
  /// window to a PNG and quits. Lets layout changes be checked headlessly.
  fn maybe_screenshot(self: &Rc<Self>) {
    let Some(path) = std::env::var_os("GPGUI_GTK_SCREENSHOT") else {
      return;
    };
    match std::env::var("GPGUI_GTK_DEMO").as_deref() {
      Ok("reconnecting") => {
        self.reconnect_failures.set(2);
        self.reconnect_remaining.set(17);
        self.set_status(VpnStatus::Reconnecting);
      }
      Ok("error") => {
        self.last_error.replace(Some(
          "Re-run it with the `--fix-openssl` option to work around this issue".into(),
        ));
        self.set_status(VpnStatus::Disconnected);
      }
      Ok("connected") => self.set_status(VpnStatus::Connected),
      Ok("log") => {
        self.append_log(LogLevel::Info, "──── Connecting to vpn.example.com ────");
        self.append_log(LogLevel::Info, "Portal prelogin succeeded");
        self.append_log(LogLevel::Warn, "Connection lost — reconnect attempt 1 in 5s");
        self.append_log(LogLevel::Error, "Re-run it with the `--fix-openssl` option");
        self.show_log();
      }
      _ => {}
    }
    let this = self.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
      let window = &this.w.window;
      let paintable = gtk::WidgetPaintable::new(Some(window));
      let snapshot = gtk::Snapshot::new();
      paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
      let texture = snapshot
        .to_node()
        .zip(window.native().and_then(|n| n.renderer()))
        .map(|(node, renderer)| renderer.render_texture(&node, None));
      match texture.map(|t| t.save_to_png(&path)) {
        Some(Ok(())) => log::info!("Screenshot saved ({}x{})", window.width(), window.height()),
        other => log::error!("Screenshot failed: {other:?}"),
      }
      this.app.quit();
    });
  }

  fn show(&self) {
    self.w.window.present();
  }

  fn toast(&self, message: &str) {
    let toast = adw::Toast::new(message);
    toast.set_timeout(6);
    self.w.toasts.add_toast(toast);
  }

  // ---- actions & signals ---------------------------------------------------

  fn install_actions(self: &Rc<Self>) {
    let weak = Rc::downgrade(self);
    let prefs = gio::ActionEntry::builder("preferences")
      .activate(move |_: &adw::Application, _, _| {
        if let Some(this) = weak.upgrade() {
          this.open_preferences();
        }
      })
      .build();

    let weak = Rc::downgrade(self);
    let stop_others = gio::ActionEntry::builder("stop-others")
      .activate(move |_: &adw::Application, _, _| {
        if let Some(this) = weak.upgrade() {
          this.stop_others();
        }
      })
      .build();

    let weak = Rc::downgrade(self);
    let show_log = gio::ActionEntry::builder("show-log")
      .activate(move |_: &adw::Application, _, _| {
        if let Some(this) = weak.upgrade() {
          this.show_log();
        }
      })
      .build();

    let weak = Rc::downgrade(self);
    let auto_reconnect = gio::ActionEntry::builder("auto-reconnect")
      .state(self.settings.borrow().auto_reconnect.to_variant())
      .activate(move |_: &adw::Application, action, _| {
        let enabled = !action.state().and_then(|v| v.get::<bool>()).unwrap_or(false);
        if let Some(this) = weak.upgrade() {
          this.set_auto_reconnect(enabled);
        }
      })
      .build();

    let weak = Rc::downgrade(self);
    let about = gio::ActionEntry::builder("about")
      .activate(move |_: &adw::Application, _, _| {
        if let Some(this) = weak.upgrade() {
          this.show_about();
        }
      })
      .build();

    let weak = Rc::downgrade(self);
    let quit = gio::ActionEntry::builder("quit")
      .activate(move |_: &adw::Application, _, _| {
        if let Some(this) = weak.upgrade() {
          this.request_quit();
        }
      })
      .build();

    self
      .app
      .add_action_entries([prefs, show_log, auto_reconnect, stop_others, about, quit]);
    self.app.set_accels_for_action("app.show-log", &["<Control>l"]);
    self.app.set_accels_for_action("app.quit", &["<Control>q"]);
    self.app.set_accels_for_action("app.preferences", &["<Control>comma"]);
  }

  fn connect_signals(self: &Rc<Self>) {
    let weak = Rc::downgrade(self);
    self.w.action.connect_clicked(move |_| {
      if let Some(this) = weak.upgrade() {
        if this.running.get() || this.status.get() == VpnStatus::Reconnecting {
          this.disconnect();
        } else {
          this.connect();
        }
      }
    });

    // Enter in any field connects.
    for row in [&self.w.portal, &self.w.gateway, &self.w.username] {
      let weak = Rc::downgrade(self);
      row.connect_entry_activated(move |_| {
        if let Some(this) = weak.upgrade() {
          this.connect();
        }
      });
    }
    let weak = Rc::downgrade(self);
    self.w.password.connect_entry_activated(move |_| {
      if let Some(this) = weak.upgrade() {
        this.connect();
      }
    });

    let weak = Rc::downgrade(self);
    self.w.recent_list.connect_row_activated(move |_, row| {
      let Some(this) = weak.upgrade() else { return };
      if let Some(label) = row.child().and_downcast::<gtk::Label>() {
        this.w.portal.set_text(&label.text());
        this.w.recent_button.popdown();
        this.load_saved_password();
      }
    });

    let weak = Rc::downgrade(self);
    self.w.window.connect_close_request(move |window| {
      let Some(this) = weak.upgrade() else {
        return glib::Propagation::Proceed;
      };
      if this.settings.borrow().close_to_tray && this.tray.is_available() {
        window.set_visible(false);
      } else {
        this.request_quit();
      }
      glib::Propagation::Stop
    });
  }

  // ---- form ---------------------------------------------------------------

  fn refresh_recent(&self) {
    let list = &self.w.recent_list;
    while let Some(child) = list.first_child() {
      list.remove(&child);
    }
    let recent = self.settings.borrow().recent_portals.clone();
    for portal in &recent {
      let label = gtk::Label::builder()
        .label(portal)
        .xalign(0.0)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
      list.append(&label);
    }
    self.w.recent_button.set_visible(!recent.is_empty());
  }

  fn load_saved_password(&self) {
    let portal = self.w.portal.text().trim().to_owned();
    let username = self.w.username.text().trim().to_owned();
    if !self.w.remember.is_active() || portal.is_empty() || username.is_empty() {
      return;
    }
    let password_row = self.w.password.clone();
    glib::spawn_future_local(async move {
      match gio::spawn_blocking(move || secrets::load(&portal, &username)).await {
        Ok(Ok(Some(password))) => password_row.set_text(&password),
        Ok(Ok(None)) => {}
        Ok(Err(err)) => log::warn!("Cannot read saved password: {err:#}"),
        Err(_) => log::warn!("Password lookup panicked"),
      }
    });
  }

  fn persist_form(&self) {
    let portal = self.w.portal.text().trim().to_owned();
    let username = self.w.username.text().trim().to_owned();
    let password = self.w.password.text().to_string();
    let remember = self.w.remember.is_active();
    {
      let mut s = self.settings.borrow_mut();
      s.profile.portal = portal.clone();
      s.profile.gateway = self.w.gateway.text().trim().to_owned();
      s.profile.username = username.clone();
      s.profile.remember_password = remember;
      s.remember_portal(&portal);
      if let Err(err) = s.save() {
        log::warn!("Cannot save settings: {err:#}");
      }
    }
    self.refresh_recent();

    if username.is_empty() {
      return;
    }
    gio::spawn_blocking(move || {
      let result = if remember && !password.is_empty() {
        secrets::store(&portal, &username, &password)
      } else if !remember {
        secrets::delete(&portal, &username)
      } else {
        Ok(())
      };
      if let Err(err) = result {
        log::warn!("Keyring update failed: {err:#}");
      }
    });
  }

  // ---- connection ---------------------------------------------------------

  /// Connects, first offering to stop VPN processes left by another client/GUI.
  fn connect(self: &Rc<Self>) {
    if self.running.get() {
      return;
    }
    self.cancel_reconnect();
    self.reconnect_failures.set(0);
    let others = vpn::find_vpn_processes(&[]);
    if others.is_empty() {
      self.start_connect();
    } else {
      self.confirm_stop_others(others, true);
    }
  }

  fn start_connect(&self) {
    if self.running.get() {
      return;
    }
    let portal = self.w.portal.text().trim().to_owned();
    if portal.is_empty() {
      self.toast("Enter a portal address");
      self.w.portal.grab_focus();
      return;
    }
    let gpclient = match vpn::resolve_gpclient(&self.settings.borrow()) {
      Ok(path) => path,
      Err(err) => {
        self.toast(&format!("{err:#}"));
        return;
      }
    };

    self.persist_form();
    let opt = |s: glib::GString| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    let request = ConnectRequest {
      portal,
      gateway: opt(self.w.gateway.text()),
      username: opt(self.w.username.text()),
      password: Some(self.w.password.text().to_string()).filter(|p| !p.is_empty()),
    };

    let verb = if self.reconnecting.get() {
      "Reconnecting to"
    } else {
      "Connecting to"
    };
    self.append_log(LogLevel::Info, &format!("──── {verb} {} ────", request.portal));
    self.was_connected.set(false);
    if !self.reconnecting.get() {
      self.last_error.replace(None);
    }
    self.user_disconnect.set(false);
    self.needs_fix_openssl.set(false);
    self.another_instance.set(false);
    let settings = self.settings.borrow().clone();
    match vpn::spawn_connect(&gpclient, &settings, request, self.vpn_tx.clone()) {
      Ok(pid) => {
        self.child_pid.set(Some(pid));
        self.running.set(true);
        self.set_status(VpnStatus::Connecting);
      }
      Err(err) => self.toast(&format!("{err:#}")),
    }
  }

  fn disconnect(self: &Rc<Self>) {
    if !self.running.get() && self.status.get() == VpnStatus::Reconnecting {
      self.cancel_reconnect();
      self.last_error.replace(None);
      self.set_status(VpnStatus::Disconnected);
      if self.quit_after_disconnect.get() {
        self.app.quit();
      }
      return;
    }
    if !self.running.get() || self.status.get() == VpnStatus::Disconnecting {
      return;
    }
    let gpclient = match vpn::resolve_gpclient(&self.settings.borrow()) {
      Ok(path) => path,
      Err(err) => {
        self.toast(&format!("{err:#}"));
        return;
      }
    };
    let previous = self.status.get();
    self.user_disconnect.set(true);
    self.set_status(VpnStatus::Disconnecting);

    let weak = Rc::downgrade(self);
    glib::spawn_future_local(async move {
      let result = gio::spawn_blocking(move || vpn::disconnect(&gpclient)).await;
      let Some(this) = weak.upgrade() else { return };
      let error = match result {
        Ok(Ok(())) => return, // the Exited event finishes the transition
        Ok(Err(err)) => format!("{err:#}"),
        Err(_) => "Disconnect panicked".to_owned(),
      };
      if this.running.get() {
        this.user_disconnect.set(false);
        this.quit_after_disconnect.set(false);
        this.set_status(previous);
      }
      this.toast(&format!("Disconnect failed: {error}"));
    });
  }

  /// Menu action: list and stop VPN processes not started by this window.
  fn stop_others(self: &Rc<Self>) {
    let own: Vec<u32> = self
      .child_pid
      .get()
      .filter(|_| self.running.get())
      .into_iter()
      .collect();
    let others = vpn::find_vpn_processes(&own);
    if others.is_empty() {
      self.toast("No other VPN processes are running");
    } else {
      self.confirm_stop_others(others, false);
    }
  }

  fn confirm_stop_others(self: &Rc<Self>, others: Vec<vpn::VpnProcess>, then_connect: bool) {
    self.show();
    let list = others
      .iter()
      .map(|p| {
        let cmd: String = p.cmdline.chars().take(80).collect();
        format!("• {} (pid {})  {}", p.name, p.pid, glib::markup_escape_text(&cmd))
      })
      .collect::<Vec<_>>()
      .join("\n");
    let dialog = adw::AlertDialog::new(
      Some("Other VPN processes are running"),
      Some(&format!(
        "They can hold the gpclient lock or the tunnel and block a new connection:\n\n{list}"
      )),
    );
    let stop_label = if then_connect {
      "Stop & Connect"
    } else {
      "Stop Processes"
    };
    dialog.add_responses(&[("cancel", "Cancel"), ("stop", stop_label)]);
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("stop"));
    dialog.set_close_response("cancel");

    let pids: Vec<u32> = others.iter().map(|p| p.pid).collect();
    let weak = Rc::downgrade(self);
    dialog.connect_response(None, move |_, response| {
      if response != "stop" {
        return;
      }
      let pids = pids.clone();
      let weak = weak.clone();
      glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || vpn::kill_processes(&pids)).await;
        let Some(this) = weak.upgrade() else { return };
        match result {
          Ok(Ok(())) => {
            let left = vpn::find_vpn_processes(&[]);
            if left.is_empty() {
              this.toast("Stopped other VPN processes");
              if then_connect {
                this.start_connect();
              }
            } else {
              this.toast(&format!("{} process(es) are still running", left.len()));
            }
          }
          Ok(Err(err)) => this.toast(&format!("Cannot stop processes: {err:#}")),
          Err(_) => this.toast("Stopping processes panicked"),
        }
      });
    });
    dialog.present(Some(&self.w.window));
  }

  fn on_vpn_event(self: &Rc<Self>, event: VpnEvent) {
    match event {
      VpnEvent::Log { level, message } => {
        if level == LogLevel::Error {
          self.last_error.replace(Some(message.clone()));
        }
        self.append_log(level, &message);
      }
      VpnEvent::Connected => {
        self.was_connected.set(true);
        self.reconnecting.set(false);
        self.reconnect_failures.set(0);
        self.last_error.replace(None);
        if self.status.get() != VpnStatus::Disconnecting {
          self.set_status(VpnStatus::Connected);
        }
      }
      VpnEvent::NeedsFixOpenssl => self.needs_fix_openssl.set(true),
      VpnEvent::AnotherInstance => self.another_instance.set(true),
      VpnEvent::Exited { success, error } => {
        self.running.set(false);
        let was_user = self.user_disconnect.replace(false);
        if !success && !was_user && self.needs_fix_openssl.get() && self.enable_fix_openssl() {
          self.toast("Server needs legacy OpenSSL mode — enabled it and retrying");
          self.set_status(VpnStatus::Disconnected);
          self.connect();
          return;
        }
        if !success && !was_user && self.another_instance.get() {
          self.set_status(VpnStatus::Disconnected);
          let others = vpn::find_vpn_processes(&[]);
          if !others.is_empty() {
            self.confirm_stop_others(others, true);
            return;
          }
        }
        let dropped = self.was_connected.replace(false);
        if !was_user && !self.quit_after_disconnect.get() && (dropped || self.reconnecting.get()) {
          // A dropped session starts a fresh series; a failed retry counts against the limit.
          let failures = if dropped { 0 } else { self.reconnect_failures.get() + 1 };
          self.reconnect_failures.set(failures);
          if let (false, Some(err)) = (success, &error) {
            self.last_error.replace(Some(err.clone()));
          }
          if self.settings.borrow().may_reconnect(failures) {
            self.schedule_reconnect();
            return;
          }
          self.reconnecting.set(false);
          if failures > 0 {
            let msg = format!("Gave up reconnecting after {failures} attempts");
            self.append_log(LogLevel::Error, &msg);
            self.last_error.replace(Some(msg.clone()));
            self.toast(&msg);
            self.set_status(VpnStatus::Disconnected);
            return;
          }
        }
        if !success && !was_user {
          let msg = error.unwrap_or_else(|| "Connection ended unexpectedly".into());
          self.last_error.replace(Some(msg.clone()));
          self.toast(&msg);
        } else {
          self.last_error.replace(None);
        }
        self.set_status(VpnStatus::Disconnected);
        if self.quit_after_disconnect.get() {
          self.app.quit();
        }
      }
    }
  }

  /// Turns on `fix_openssl`; returns false if it was already on (no point retrying).
  fn enable_fix_openssl(&self) -> bool {
    let mut s = self.settings.borrow_mut();
    if s.fix_openssl {
      return false;
    }
    s.fix_openssl = true;
    if let Err(err) = s.save() {
      log::warn!("Cannot save settings: {err:#}");
    }
    true
  }

  // ---- auto reconnect ---------------------------------------------------

  fn set_auto_reconnect(&self, enabled: bool) {
    {
      let mut s = self.settings.borrow_mut();
      s.auto_reconnect = enabled;
      if let Err(err) = s.save() {
        log::warn!("Cannot save settings: {err:#}");
      }
    }
    self.sync_auto_reconnect_action();
    if !enabled && !self.running.get() && self.status.get() == VpnStatus::Reconnecting {
      self.cancel_reconnect();
      self.set_status(VpnStatus::Disconnected);
    }
  }

  fn sync_auto_reconnect_action(&self) {
    let enabled = self.settings.borrow().auto_reconnect;
    if let Some(action) = self
      .app
      .lookup_action("auto-reconnect")
      .and_downcast::<gio::SimpleAction>()
    {
      action.set_state(&enabled.to_variant());
    }
  }

  /// Waits the back-off delay (with a visible countdown), then reconnects.
  fn schedule_reconnect(self: &Rc<Self>) {
    self.cancel_reconnect();
    let attempt = self.reconnect_failures.get() + 1;
    let delay = self.settings.borrow().reconnect_backoff(attempt);
    self.append_log(
      LogLevel::Warn,
      &format!("Connection lost — reconnect attempt {attempt} in {delay}s"),
    );
    self.reconnect_remaining.set(delay);
    self.set_status(VpnStatus::Reconnecting);

    let weak = Rc::downgrade(self);
    let id = glib::timeout_add_seconds_local(1, move || {
      let Some(this) = weak.upgrade() else {
        return glib::ControlFlow::Break;
      };
      let left = this.reconnect_remaining.get().saturating_sub(1);
      this.reconnect_remaining.set(left);
      if left > 0 {
        this.set_status(VpnStatus::Reconnecting);
        return glib::ControlFlow::Continue;
      }
      this.reconnect_timer.replace(None);
      this.reconnecting.set(true);
      this.start_connect();
      if !this.running.get() {
        // Could not even start gpclient (e.g. it was uninstalled): stop here.
        this.reconnecting.set(false);
        this.set_status(VpnStatus::Disconnected);
      }
      glib::ControlFlow::Break
    });
    self.reconnect_timer.replace(Some(id));
  }

  fn cancel_reconnect(&self) {
    if let Some(id) = self.reconnect_timer.take() {
      id.remove();
    }
    self.reconnecting.set(false);
  }

  fn set_status(&self, status: VpnStatus) {
    self.status.set(status);
    let w = &self.w;
    let portal = self.settings.borrow().profile.portal.clone();
    let error = self.last_error.borrow().clone();
    let failed = error.is_some();
    let attempt = self.reconnect_failures.get() + 1;

    let detail = match status {
      VpnStatus::Disconnected => error.clone().unwrap_or_else(|| "Not connected".to_owned()),
      VpnStatus::Connecting if self.reconnecting.get() => {
        format!("Reconnecting to {portal} (attempt {attempt})…")
      }
      VpnStatus::Connecting => format!("Signing in to {portal}…"),
      VpnStatus::Reconnecting => format!(
        "Retrying {portal} in {}s (attempt {attempt})",
        self.reconnect_remaining.get()
      ),
      VpnStatus::Connected | VpnStatus::Disconnecting => portal.clone(),
    };
    w.status_icon.set_icon_name(Some(status.icon_name(failed)));
    w.status_title.set_label(status.label());
    w.status_detail.set_label(&detail);
    w.status_detail.set_tooltip_text(Some(&detail));
    if failed && status == VpnStatus::Disconnected {
      w.status_detail.add_css_class("error");
    } else {
      w.status_detail.remove_css_class("error");
    }

    let active = self.running.get() || status == VpnStatus::Reconnecting;
    w.form.set_sensitive(!active);
    w.remember.set_sensitive(!active);
    w.action.set_sensitive(status != VpnStatus::Disconnecting);
    let (label, add, remove) = if active {
      let label = match status {
        VpnStatus::Connecting | VpnStatus::Reconnecting => "Cancel",
        _ => "Disconnect",
      };
      (label, "destructive-action", "suggested-action")
    } else {
      ("Connect", "suggested-action", "destructive-action")
    };
    w.action.set_label(label);
    w.action.remove_css_class(remove);
    w.action.add_css_class(add);

    w.window.set_title(Some(&format!("GlobalProtect — {}", status.label())));
    self.tray.update(status, failed, &portal);
  }

  // ---- log ----------------------------------------------------------------

  fn append_log(&self, level: LogLevel, message: &str) {
    let buffer = &self.log;
    let prefix = match level {
      LogLevel::Error => "ERROR ",
      LogLevel::Warn => "WARN  ",
      LogLevel::Info => "",
      LogLevel::Debug => "DEBUG ",
    };
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("{prefix}{message}\n"));

    let overflow = buffer.line_count() - MAX_LOG_LINES;
    if overflow > 0 {
      let mut start = buffer.start_iter();
      if let Some(mut cut) = buffer.iter_at_line(overflow) {
        buffer.delete(&mut start, &mut cut);
      }
    }
    self.scroll_log_to_end();
  }

  fn scroll_log_to_end(&self) {
    let view = self.log_view.borrow().as_ref().and_then(|weak| weak.upgrade());
    if let Some(view) = view {
      let mut end = self.log.end_iter();
      view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
    }
  }

  fn show_log(self: &Rc<Self>) {
    let view = gtk::TextView::builder()
      .buffer(&self.log)
      .editable(false)
      .cursor_visible(false)
      .monospace(true)
      .wrap_mode(gtk::WrapMode::WordChar)
      .top_margin(8)
      .bottom_margin(8)
      .left_margin(10)
      .right_margin(10)
      .build();
    let scroller = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();

    let copy = gtk::Button::builder()
      .icon_name("edit-copy-symbolic")
      .tooltip_text("Copy log")
      .build();
    let buffer = self.log.clone();
    let toasts = adw::ToastOverlay::new();
    let toasts_ref = toasts.clone();
    copy.connect_clicked(move |button| {
      let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
      button.clipboard().set_text(&text);
      toasts_ref.add_toast(adw::Toast::new("Log copied"));
    });
    let clear = gtk::Button::builder()
      .icon_name("user-trash-symbolic")
      .tooltip_text("Clear log")
      .build();
    let buffer = self.log.clone();
    clear.connect_clicked(move |_| buffer.set_text(""));

    let header = adw::HeaderBar::new();
    header.pack_start(&copy);
    header.pack_start(&clear);
    toasts.set_child(Some(&scroller));
    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.set_content(Some(&toasts));

    let dialog = adw::Dialog::builder()
      .title("Connection Log")
      .content_width(680)
      .content_height(460)
      .child(&content)
      .build();
    self.log_view.replace(Some(view.downgrade()));
    dialog.present(Some(&self.w.window));
    let weak = Rc::downgrade(self);
    glib::idle_add_local_once(move || {
      if let Some(this) = weak.upgrade() {
        this.scroll_log_to_end();
      }
    });
  }

  // ---- dialogs & quit -----------------------------------------------------

  fn open_preferences(self: &Rc<Self>) {
    let current = self.settings.borrow().clone();
    let weak = Rc::downgrade(self);
    preferences::present(&self.w.window, current, move |updated| {
      let Some(this) = weak.upgrade() else { return };
      let mut s = this.settings.borrow_mut();
      // The main form owns profile/recent portals; keep them.
      let profile = std::mem::take(&mut s.profile);
      let recent = std::mem::take(&mut s.recent_portals);
      *s = Settings {
        profile,
        recent_portals: recent,
        ..updated
      };
      if let Err(err) = s.save() {
        log::warn!("Cannot save settings: {err:#}");
      }
      let enabled = s.auto_reconnect;
      drop(s);
      this.set_auto_reconnect(enabled);
    });
  }

  fn show_about(&self) {
    let about = adw::AboutDialog::builder()
      .application_name("GlobalProtect VPN")
      .application_icon(icons::APP_ICON)
      .version(env!("CARGO_PKG_VERSION"))
      .license_type(gtk::License::Gpl30)
      .comments("Open-source GTK frontend for GlobalProtect-openconnect (gpclient).")
      .website("https://github.com/yuezk/GlobalProtect-openconnect")
      .build();
    about.present(Some(&self.w.window));
  }

  fn request_quit(self: &Rc<Self>) {
    if !self.running.get() {
      self.app.quit();
      return;
    }
    self.show();
    let dialog = adw::AlertDialog::new(
      Some("Disconnect and quit?"),
      Some("The VPN is still running. It will be disconnected before the application quits."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("quit", "Disconnect & Quit")]);
    dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = Rc::downgrade(self);
    dialog.connect_response(None, move |_, response| {
      let Some(this) = weak.upgrade() else { return };
      if response == "quit" {
        this.quit_after_disconnect.set(true);
        this.disconnect();
      }
    });
    dialog.present(Some(&self.w.window));
  }
}

/// Makes the bundled icons available to GTK by name.
fn load_icons(icon_root: Option<&std::path::Path>) {
  if let (Some(root), Some(display)) = (icon_root, gtk::gdk::Display::default()) {
    gtk::IconTheme::for_display(&display).add_search_path(root);
  }
  gtk::Window::set_default_icon_name(icons::APP_ICON);

  // Newer libadwaita draws destructive buttons tinted, which reads as disabled
  // next to the solid "Connect"; keep the primary button solid in both states.
  if let Some(display) = gtk::gdk::Display::default() {
    let css = gtk::CssProvider::new();
    css.load_from_string(
      "button.vpn-action.destructive-action { background-color: @destructive_bg_color; color: @destructive_fg_color; }",
    );
    gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
  }
}

/// Single-page layout sized to its content (no scrolling):
/// status header, sign-in form, then [Remember password ……… Connect].
fn build_widgets(app: &adw::Application, settings: &Settings) -> Widgets {
  let p = &settings.profile;

  // Status header
  let status_icon = gtk::Image::builder()
    .icon_name(VpnStatus::Disconnected.icon_name(false))
    .pixel_size(64)
    .build();
  let status_title = gtk::Label::builder()
    .label(VpnStatus::Disconnected.label())
    .css_classes(["title-2"])
    .build();
  let status_detail = gtk::Label::builder()
    .label("Not connected")
    .css_classes(["dim-label"])
    .justify(gtk::Justification::Center)
    .wrap(true)
    .wrap_mode(gtk::pango::WrapMode::WordChar)
    .lines(2)
    .ellipsize(gtk::pango::EllipsizeMode::End)
    .max_width_chars(42)
    .build();
  let status_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
  status_box.append(&status_icon);
  status_box.append(&status_title);
  status_box.append(&status_detail);

  // Form; the portal row has a "recent portals" popover.
  let portal = adw::EntryRow::builder().title("Portal").text(&p.portal).build();
  let recent_list = gtk::ListBox::builder()
    .selection_mode(gtk::SelectionMode::None)
    .css_classes(["navigation-sidebar"])
    .build();
  let recent_button = gtk::MenuButton::builder()
    .icon_name("document-open-recent-symbolic")
    .tooltip_text("Recent portals")
    .valign(gtk::Align::Center)
    .css_classes(["flat"])
    .popover(&gtk::Popover::builder().child(&recent_list).build())
    .build();
  portal.add_suffix(&recent_button);

  let gateway = adw::EntryRow::builder()
    .title("Gateway (optional)")
    .text(&p.gateway)
    .build();
  let username = adw::EntryRow::builder().title("Username").text(&p.username).build();
  let password = adw::PasswordEntryRow::builder()
    .title("Password (empty for SSO)")
    .build();

  let form = adw::PreferencesGroup::new();
  form.add(&portal);
  form.add(&gateway);
  form.add(&username);
  form.add(&password);

  // Footer: remember + primary action
  let remember = gtk::CheckButton::builder()
    .label("Remember password")
    .tooltip_text("Store the password in the system keyring")
    .active(p.remember_password)
    .hexpand(true)
    .valign(gtk::Align::Center)
    .build();
  let action = gtk::Button::builder()
    .label("Connect")
    .width_request(140)
    .css_classes(["pill", "suggested-action", "vpn-action"])
    .build();
  let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
  footer.append(&remember);
  footer.append(&action);

  let body = gtk::Box::builder()
    .orientation(gtk::Orientation::Vertical)
    .spacing(18)
    .margin_top(6)
    .margin_bottom(24)
    .margin_start(18)
    .margin_end(18)
    .build();
  body.append(&status_box);
  body.append(&form);
  body.append(&footer);

  let menu = gio::Menu::new();
  let connection = gio::Menu::new();
  connection.append(Some("_Auto Reconnect"), Some("app.auto-reconnect"));
  connection.append(Some("Show _Log"), Some("app.show-log"));
  connection.append(Some("_Stop Other VPN Processes…"), Some("app.stop-others"));
  menu.append_section(None, &connection);
  let general = gio::Menu::new();
  general.append(Some("_Preferences"), Some("app.preferences"));
  general.append(Some("_About"), Some("app.about"));
  general.append(Some("_Quit"), Some("app.quit"));
  menu.append_section(None, &general);
  let menu_button = gtk::MenuButton::builder()
    .icon_name("open-menu-symbolic")
    .menu_model(&menu)
    .primary(true)
    .tooltip_text("Main Menu")
    .build();
  let header = adw::HeaderBar::builder().show_title(false).build();
  header.pack_end(&menu_button);

  let toasts = adw::ToastOverlay::new();
  toasts.set_child(Some(&adw::Clamp::builder().maximum_size(440).child(&body).build()));

  let view = adw::ToolbarView::new();
  view.add_top_bar(&header);
  view.set_content(Some(&toasts));

  // No default height: the window takes the natural height of its content.
  let window = adw::ApplicationWindow::builder()
    .application(app)
    .title("GlobalProtect")
    .default_width(420)
    .content(&view)
    .build();
  // Start on the button so the portal text is not pre-selected.
  GtkWindowExt::set_focus(&window, Some(&action));

  Widgets {
    window,
    toasts,
    status_icon,
    status_title,
    status_detail,
    form,
    portal,
    recent_list,
    recent_button,
    gateway,
    username,
    password,
    remember,
    action,
  }
}
