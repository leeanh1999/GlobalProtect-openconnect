use std::{
  cell::{Cell, RefCell},
  rc::Rc,
};

use adw::prelude::*;
use gpapi::service::vpn_state::VpnState;
use gtk::{gio, glib};
use tokio_util::sync::CancellationToken;

use crate::{
  auth::{self, AuthInput, AuthUi},
  config::Settings,
  icons, preferences, secrets,
  service::{self, ServiceCommand, ServiceEvent, ServiceHandle},
  state::VpnStatus,
  tray::{TrayCommand, TrayHandle},
  vpn::{self, LogLevel},
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

/// Outcome of one sign-in + Connect attempt, reported back to the GTK thread.
enum AttemptEvent {
  Log(LogLevel, String),
  /// The service accepted the Connect request for this gateway.
  Accepted(String),
  Failed {
    error: String,
    cancelled: bool,
  },
  /// MFA prompt; reply on the channel (`None` = cancel).
  Otp(String, async_channel::Sender<Option<String>>),
}

pub struct App {
  app: adw::Application,
  w: Widgets,
  settings: RefCell<Settings>,
  status: Cell<VpnStatus>,
  /// Tokio runtime for networking (auth, gpservice transport).
  rt: tokio::runtime::Runtime,
  service: RefCell<Option<ServiceHandle>>,
  service_tx: async_channel::Sender<ServiceEvent>,
  attempt_tx: async_channel::Sender<AttemptEvent>,
  /// Sign-in in progress; cancelling it aborts gpauth/HTTP requests.
  auth_cancel: RefCell<Option<CancellationToken>>,
  /// gpservice owns a tunnel attempt we started (Connect accepted).
  tunnel_requested: Cell<bool>,
  gateway_name: RefCell<String>,
  /// `restart_of` for the next Connect, when gpservice asked for re-authentication.
  restart_of: Cell<Option<u64>>,
  user_disconnect: Cell<bool>,
  last_error: RefCell<Option<String>>,
  /// Connection log, shown on demand from the menu.
  log: gtk::TextBuffer,
  log_view: RefCell<Option<glib::WeakRef<gtk::TextView>>>,
  /// The current attempt reached "Connected".
  was_connected: Cell<bool>,
  /// The current attempt is an automatic reconnect.
  reconnecting: Cell<bool>,
  /// Failed reconnect attempts in a row.
  reconnect_failures: Cell<u32>,
  reconnect_remaining: Cell<u32>,
  reconnect_timer: RefCell<Option<glib::SourceId>>,
  tray: TrayHandle,
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

    let rt = tokio::runtime::Builder::new_multi_thread()
      .worker_threads(2)
      .enable_all()
      .build()
      .expect("tokio runtime");
    let (service_tx, service_rx) = async_channel::unbounded::<ServiceEvent>();
    let (attempt_tx, attempt_rx) = async_channel::unbounded::<AttemptEvent>();
    let (tray_tx, tray_rx) = async_channel::unbounded::<TrayCommand>();
    let tray = TrayHandle::spawn(tray_tx, icons::tray_pixmaps());

    let this = Rc::new(Self {
      app: app.clone(),
      w,
      settings: RefCell::new(settings),
      status: Cell::new(VpnStatus::Disconnected),
      rt,
      service: RefCell::default(),
      service_tx,
      attempt_tx,
      auth_cancel: RefCell::default(),
      tunnel_requested: Cell::new(false),
      gateway_name: RefCell::default(),
      restart_of: Cell::new(None),
      user_disconnect: Cell::new(false),
      last_error: RefCell::default(),
      log: gtk::TextBuffer::new(None),
      log_view: RefCell::default(),
      was_connected: Cell::new(false),
      reconnecting: Cell::new(false),
      reconnect_failures: Cell::new(0),
      reconnect_remaining: Cell::new(0),
      reconnect_timer: RefCell::default(),
      tray,
      _hold: RefCell::new(Some(app.hold())),
    });

    this.install_actions();
    this.connect_signals();
    this.refresh_recent();
    this.load_saved_password();
    this.set_status(VpnStatus::Disconnected);

    let weak = Rc::downgrade(&this);
    glib::spawn_future_local(async move {
      while let Ok(event) = service_rx.recv().await {
        let Some(this) = weak.upgrade() else { break };
        this.on_service_event(event);
      }
    });

    let weak = Rc::downgrade(&this);
    glib::spawn_future_local(async move {
      while let Ok(event) = attempt_rx.recv().await {
        let Some(this) = weak.upgrade() else { break };
        this.on_attempt_event(event);
      }
    });

    let weak = Rc::downgrade(&this);
    glib::spawn_future_local(async move {
      while let Ok(cmd) = tray_rx.recv().await {
        let Some(this) = weak.upgrade() else { break };
        match cmd {
          TrayCommand::ShowWindow => this.show(),
          // Connect in the background; the window only opens when input is needed.
          TrayCommand::Connect => this.connect(),
          TrayCommand::Disconnect => this.disconnect(),
          TrayCommand::Quit => this.request_quit(),
        }
      }
    });

    this
  }

  /// Signing in, or gpservice busy with a tunnel we asked for.
  fn is_active(&self) -> bool {
    self.auth_cancel.borrow().is_some()
      || self.tunnel_requested.get()
      || matches!(
        self.status.get(),
        VpnStatus::Connected | VpnStatus::Disconnecting | VpnStatus::Reconnecting
      )
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
    // Hidden window (tray-only use): toasts would go unseen, use a desktop notification.
    if !self.w.window.is_visible() {
      let notification = gio::Notification::new("GlobalProtect VPN");
      notification.set_body(Some(message));
      self.app.send_notification(Some("status"), &notification);
    }
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
        if this.is_active() {
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
    if self.is_active() {
      return;
    }
    self.cancel_reconnect();
    self.reconnect_failures.set(0);
    self.restart_of.set(None);
    let own = self.own_service_pid();
    let others = vpn::find_vpn_processes(&own);
    if others.is_empty() {
      self.start_connect();
    } else {
      self.confirm_stop_others(others, true);
    }
  }

  fn own_service_pid(&self) -> Vec<u32> {
    self
      .service
      .borrow()
      .as_ref()
      .filter(|s| s.is_alive())
      .and_then(|s| s.pid)
      .into_iter()
      .collect()
  }

  /// Starts gpservice on first use (pkexec; no password with the upstream polkit policy).
  fn ensure_service(&self) -> anyhow::Result<tokio::sync::mpsc::UnboundedSender<ServiceCommand>> {
    if let Some(handle) = self.service.borrow().as_ref().filter(|s| s.is_alive()) {
      return Ok(handle.sender());
    }
    let path = service::resolve_gpservice(&self.settings.borrow().gpservice_path)?;
    self.append_log(LogLevel::Info, &format!("Starting {}", path.display()));
    let handle = service::start(self.rt.handle(), path, self.service_tx.clone())?;
    let sender = handle.sender();
    self.service.replace(Some(handle));
    Ok(sender)
  }

  /// Signs in (in the user session) and asks gpservice to open the tunnel.
  fn start_connect(&self) {
    if self.auth_cancel.borrow().is_some() || self.tunnel_requested.get() {
      return;
    }
    let portal = self.w.portal.text().trim().to_owned();
    if portal.is_empty() {
      self.show();
      self.toast("Enter a portal address");
      self.w.portal.grab_focus();
      return;
    }
    let service = match self.ensure_service() {
      Ok(service) => service,
      Err(err) => {
        self.toast(&format!("{err:#}"));
        return;
      }
    };

    self.persist_form();
    let opt = |s: glib::GString| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    let input = AuthInput {
      portal: portal.clone(),
      gateway: opt(self.w.gateway.text()),
      username: opt(self.w.username.text()),
      password: Some(self.w.password.text().to_string()).filter(|p| !p.is_empty()),
      settings: self.settings.borrow().clone(),
    };

    let verb = if self.reconnecting.get() {
      "Reconnecting to"
    } else {
      "Connecting to"
    };
    self.append_log(LogLevel::Info, &format!("──── {verb} {portal} ────"));
    self.was_connected.set(false);
    self.user_disconnect.set(false);
    if !self.reconnecting.get() {
      self.last_error.replace(None);
    }
    self.gateway_name.replace(String::new());
    let cancel = CancellationToken::new();
    self.auth_cancel.replace(Some(cancel.clone()));
    self.set_status(VpnStatus::Connecting);

    let restart_of = self.restart_of.take();
    let events = self.attempt_tx.clone();
    self.rt.spawn(async move {
      let log_tx = events.clone();
      let otp_tx = events.clone();
      let ui = AuthUi {
        log: Box::new(move |msg| {
          let _ = log_tx.try_send(AttemptEvent::Log(LogLevel::Info, msg));
        }),
        ask_otp: Box::new(move |message| {
          let (reply, rx) = async_channel::bounded(1);
          let _ = otp_tx.try_send(AttemptEvent::Otp(message, reply));
          rx
        }),
      };
      let outcome = async {
        let authenticated = auth::authenticate(&input, &ui, &cancel).await?;
        let gateway = authenticated.gateway_name.clone();
        let request = authenticated.request.with_restart_of(restart_of);
        let (reply, accepted) = tokio::sync::oneshot::channel();
        ui_log(&events, format!("Asking gpservice to connect to {gateway}"));
        let sent = service.send(ServiceCommand::Connect(Box::new(request), reply)).is_ok();
        let result = if sent {
          accepted
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("gpservice stopped")))
        } else {
          Err(anyhow::anyhow!("gpservice is not running"))
        };
        match result {
          // The service now owns the gateway sessions (and logs them out itself).
          Ok(()) => {
            authenticated.sessions.relinquish();
            Ok(gateway)
          }
          Err(err) => {
            authenticated.sessions.logout().await;
            Err(err.context("gpservice refused the connection"))
          }
        }
      }
      .await;
      let event = match outcome {
        Ok(gateway) => AttemptEvent::Accepted(gateway),
        Err(err) => AttemptEvent::Failed {
          cancelled: err.downcast_ref::<gpapi::auth::AuthenticationCancelled>().is_some() || cancel.is_cancelled(),
          error: format!("{err:#}"),
        },
      };
      let _ = events.send(event).await;
    });
  }

  fn disconnect(self: &Rc<Self>) {
    match self.status.get() {
      VpnStatus::Reconnecting => {
        self.cancel_reconnect();
        self.last_error.replace(None);
        self.set_status(VpnStatus::Disconnected);
        return;
      }
      VpnStatus::Disconnecting => return,
      _ => {}
    }
    self.user_disconnect.set(true);
    if let Some(cancel) = self.auth_cancel.borrow().as_ref() {
      self.append_log(LogLevel::Info, "Sign-in cancelled");
      cancel.cancel();
    }
    if self.tunnel_requested.get() || self.status.get() == VpnStatus::Connected {
      let sent = self
        .service
        .borrow()
        .as_ref()
        .is_some_and(|s| s.send(ServiceCommand::Disconnect));
      if sent {
        self.set_status(VpnStatus::Disconnecting);
      } else {
        self.tunnel_requested.set(false);
        self.set_status(VpnStatus::Disconnected);
      }
    }
  }

  /// Menu action: list and stop VPN processes not started by this window.
  fn stop_others(self: &Rc<Self>) {
    let others = vpn::find_vpn_processes(&self.own_service_pid());
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
        "They can hold the gpservice/gpclient lock or the tunnel and block a new connection:\n\n{list}"
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
            let left = vpn::find_vpn_processes(&this.own_service_pid());
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

  // ---- events -------------------------------------------------------------

  fn on_attempt_event(self: &Rc<Self>, event: AttemptEvent) {
    match event {
      AttemptEvent::Log(level, message) => self.append_log(level, &message),
      AttemptEvent::Otp(message, reply) => self.ask_otp(&message, reply),
      AttemptEvent::Accepted(gateway) => {
        self.auth_cancel.replace(None);
        self.tunnel_requested.set(true);
        self.append_log(
          LogLevel::Info,
          &format!("gpservice accepted the connection to {gateway}"),
        );
        self.gateway_name.replace(gateway);
        if self.status.get() != VpnStatus::Connected {
          self.set_status(VpnStatus::Connecting);
        }
      }
      AttemptEvent::Failed { error, cancelled } => {
        self.auth_cancel.replace(None);
        let was_user = self.user_disconnect.replace(false) || cancelled;
        self.append_log(
          if was_user { LogLevel::Info } else { LogLevel::Error },
          &if was_user {
            "Sign-in cancelled".to_owned()
          } else {
            error.clone()
          },
        );
        if was_user {
          self.reconnecting.set(false);
          self.last_error.replace(None);
          self.set_status(VpnStatus::Disconnected);
        } else if is_legacy_tls_error(&error) && self.enable_fix_openssl() {
          self.set_status(VpnStatus::Disconnected);
          self.offer_restart_for_openssl();
        } else {
          self.attempt_failed(error);
        }
      }
    }
  }

  fn on_service_event(self: &Rc<Self>, event: ServiceEvent) {
    match event {
      ServiceEvent::Log(level, message) => self.append_log(level, &message),
      ServiceEvent::Ready(state) => {
        self.append_log(LogLevel::Info, "Attached to gpservice");
        self.apply_vpn_state(state);
      }
      ServiceEvent::State(state) => self.apply_vpn_state(state),
      ServiceEvent::Stopped(reason) => {
        self.append_log(LogLevel::Warn, &format!("gpservice stopped: {reason}"));
        self.service.replace(None);
        if let Some(cancel) = self.auth_cancel.borrow().as_ref() {
          cancel.cancel();
        }
        let was_active = self.tunnel_requested.replace(false) || self.status.get() == VpnStatus::Connected;
        if was_active || self.status.get() == VpnStatus::Connecting {
          self.tunnel_ended(Some(reason));
        }
      }
    }
  }

  fn apply_vpn_state(self: &Rc<Self>, state: VpnState) {
    match state {
      VpnState::Connecting(info) => {
        if self.tunnel_requested.get() {
          self.gateway_name.replace(info.gateway().name().to_owned());
          self.set_status(VpnStatus::Connecting);
        }
      }
      VpnState::Connected(connected) => {
        self.tunnel_requested.set(true);
        self.was_connected.set(true);
        self.reconnecting.set(false);
        self.reconnect_failures.set(0);
        self.last_error.replace(None);
        self.gateway_name.replace(connected.info().gateway().name().to_owned());
        self.append_log(
          LogLevel::Info,
          &format!("Connected to {}", connected.info().gateway().name()),
        );
        self.set_status(VpnStatus::Connected);
      }
      VpnState::Disconnecting => {
        if self.tunnel_requested.get() {
          self.set_status(VpnStatus::Disconnecting);
        }
      }
      VpnState::Disconnected => {
        if self.tunnel_requested.replace(false) {
          self.tunnel_ended(None);
        }
      }
      VpnState::Failed(message) => {
        if self.tunnel_requested.replace(false) {
          self.tunnel_ended(Some(message));
        }
      }
      VpnState::ReconnectRequired(request) => {
        // gpservice lost the tunnel and needs fresh credentials to resume it.
        self.tunnel_requested.set(false);
        self.append_log(LogLevel::Warn, "gpservice requests re-authentication");
        if self.settings.borrow().auto_reconnect && !self.user_disconnect.get() {
          self.restart_of.set(Some(request.attempt));
          self.was_connected.set(false);
          self.reconnecting.set(true);
          self.start_connect();
        } else {
          self.tunnel_ended(Some("The connection needs to be re-authenticated".into()));
        }
      }
    }
  }

  /// The tunnel we requested is gone (user disconnect, drop, or failure).
  fn tunnel_ended(self: &Rc<Self>, error: Option<String>) {
    let was_user = self.user_disconnect.replace(false);
    let dropped = self.was_connected.replace(false);
    if was_user {
      self.reconnecting.set(false);
      self.last_error.replace(None);
      self.set_status(VpnStatus::Disconnected);
      return;
    }
    let error = error.unwrap_or_else(|| "Connection ended unexpectedly".into());
    if let Some(err) = Some(&error) {
      self.append_log(LogLevel::Warn, err);
    }
    if dropped {
      // A dropped session starts a fresh reconnect series.
      self.reconnect_failures.set(0);
      self.last_error.replace(Some(error.clone()));
      if self.settings.borrow().may_reconnect(0) {
        self.schedule_reconnect();
        return;
      }
      self.toast(&error);
      self.set_status(VpnStatus::Disconnected);
    } else {
      self.attempt_failed(error);
    }
  }

  /// An attempt that never reached Connected failed.
  fn attempt_failed(self: &Rc<Self>, error: String) {
    self.last_error.replace(Some(error.clone()));
    if self.reconnecting.get() {
      let failures = self.reconnect_failures.get() + 1;
      self.reconnect_failures.set(failures);
      if self.settings.borrow().may_reconnect(failures) {
        self.schedule_reconnect();
        return;
      }
      self.reconnecting.set(false);
      let msg = format!("Gave up reconnecting after {failures} attempts: {error}");
      self.append_log(LogLevel::Error, &msg);
      self.last_error.replace(Some(msg.clone()));
      self.toast(&msg);
    } else {
      self.toast(&error);
    }
    self.set_status(VpnStatus::Disconnected);
  }

  fn ask_otp(&self, message: &str, reply: async_channel::Sender<Option<String>>) {
    self.show();
    let entry = gtk::PasswordEntry::builder()
      .show_peek_icon(true)
      .activates_default(true)
      .build();
    let dialog = adw::AlertDialog::new(Some("Verification code"), Some(message));
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Continue")]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
      let code = (response == "ok").then(|| entry.text().to_string());
      let _ = reply.try_send(code);
    });
    dialog.present(Some(&self.w.window));
  }

  /// Turns on `fix_openssl`; returns false if it was already on.
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

  /// OpenSSL reads its configuration once per process, so the legacy mode needs a restart.
  fn offer_restart_for_openssl(&self) {
    self.append_log(
      LogLevel::Warn,
      "Server needs legacy TLS renegotiation; enabled OpenSSL compatibility mode",
    );
    let dialog = adw::AlertDialog::new(
      Some("Restart required"),
      Some(
        "The server needs legacy OpenSSL compatibility. It has been enabled and takes effect after restarting the app.",
      ),
    );
    dialog.add_responses(&[("later", "Later"), ("restart", "Restart Now")]);
    dialog.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("restart"));
    dialog.set_close_response("later");
    let app = self.app.clone();
    dialog.connect_response(None, move |_, response| {
      if response == "restart" {
        if let Ok(exe) = std::env::current_exe() {
          let _ = std::process::Command::new(exe).spawn();
        }
        app.quit();
      }
    });
    dialog.present(Some(&self.w.window));
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
    if !enabled && self.status.get() == VpnStatus::Reconnecting {
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
      if this.auth_cancel.borrow().is_none() {
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
      VpnStatus::Connecting if self.auth_cancel.borrow().is_some() => format!("Signing in to {portal}…"),
      VpnStatus::Connecting => format!("Connecting to {}…", self.gateway_name.borrow()),
      VpnStatus::Reconnecting => format!(
        "Retrying {portal} in {}s (attempt {attempt})",
        self.reconnect_remaining.get()
      ),
      VpnStatus::Connected | VpnStatus::Disconnecting => {
        let gateway = self.gateway_name.borrow();
        if gateway.is_empty() {
          portal.clone()
        } else {
          format!("{portal} · {gateway}")
        }
      }
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

    let active = self.is_active() || status == VpnStatus::Connecting;
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
    if !self.is_active() {
      self.app.quit();
      return;
    }
    self.show();
    let dialog = adw::AlertDialog::new(
      Some("Disconnect and quit?"),
      Some("The VPN is still running. Quitting stops gpservice, which disconnects the VPN."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("quit", "Disconnect & Quit")]);
    dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = Rc::downgrade(self);
    dialog.connect_response(None, move |_, response| {
      let Some(this) = weak.upgrade() else { return };
      if response == "quit" {
        if let Some(cancel) = this.auth_cancel.borrow().as_ref() {
          cancel.cancel();
        }
        // Dropping the service handle closes gpservice's stdin: it disconnects and exits.
        this.service.replace(None);
        this.app.quit();
      }
    });
    dialog.present(Some(&self.w.window));
  }
}

fn ui_log(events: &async_channel::Sender<AttemptEvent>, message: String) {
  let _ = events.try_send(AttemptEvent::Log(LogLevel::Info, message));
}

/// OpenSSL "unsafe legacy renegotiation disabled" (servers needing `--fix-openssl`).
fn is_legacy_tls_error(error: &str) -> bool {
  let e = error.to_ascii_lowercase();
  e.contains("legacy renegotiation") || e.contains("legacy_renegotiation")
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

#[cfg(test)]
mod tests {
  use super::is_legacy_tls_error;

  #[test]
  fn detects_openssl_legacy_renegotiation_errors() {
    assert!(is_legacy_tls_error(
      "error:0A000152:SSL routines:final_renegotiate:unsafe legacy renegotiation disabled"
    ));
    assert!(!is_legacy_tls_error("certificate verify failed"));
  }
}
