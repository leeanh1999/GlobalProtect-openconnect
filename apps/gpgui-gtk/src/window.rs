use std::{
  cell::{Cell, RefCell},
  rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
  config::Settings,
  preferences, secrets,
  state::VpnStatus,
  tray::{TrayCommand, TrayHandle},
  vpn::{self, ConnectRequest, LogLevel, VpnEvent},
};

const MAX_LOG_LINES: i32 = 2000;

struct Widgets {
  window: adw::ApplicationWindow,
  toasts: adw::ToastOverlay,
  status: adw::StatusPage,
  form: adw::PreferencesGroup,
  portal: adw::EntryRow,
  recent_list: gtk::ListBox,
  recent_button: gtk::MenuButton,
  gateway: adw::EntryRow,
  username: adw::EntryRow,
  password: adw::PasswordEntryRow,
  remember: adw::SwitchRow,
  action: gtk::Button,
  log_view: gtk::TextView,
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
  last_error: RefCell<Option<String>>,
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
    instance.replace(Some(created));
  });
}

impl App {
  fn new(app: &adw::Application) -> Rc<Self> {
    let settings = Settings::load();
    let w = build_widgets(app, &settings);

    let (vpn_tx, vpn_rx) = async_channel::unbounded::<VpnEvent>();
    let (tray_tx, tray_rx) = async_channel::unbounded::<TrayCommand>();
    let tray = TrayHandle::spawn(tray_tx);

    let this = Rc::new(Self {
      app: app.clone(),
      w,
      settings: RefCell::new(settings),
      status: Cell::new(VpnStatus::Disconnected),
      running: Cell::new(false),
      user_disconnect: Cell::new(false),
      quit_after_disconnect: Cell::new(false),
      last_error: RefCell::default(),
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

    self.app.add_action_entries([prefs, about, quit]);
    self.app.set_accels_for_action("app.quit", &["<Control>q"]);
    self.app.set_accels_for_action("app.preferences", &["<Control>comma"]);
  }

  fn connect_signals(self: &Rc<Self>) {
    let weak = Rc::downgrade(self);
    self.w.action.connect_clicked(move |_| {
      if let Some(this) = weak.upgrade() {
        if this.running.get() {
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

  fn connect(&self) {
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

    self.clear_log();
    self.last_error.replace(None);
    self.user_disconnect.set(false);
    let settings = self.settings.borrow().clone();
    match vpn::spawn_connect(&gpclient, &settings, request, self.vpn_tx.clone()) {
      Ok(()) => {
        self.running.set(true);
        self.set_status(VpnStatus::Connecting);
      }
      Err(err) => self.toast(&format!("{err:#}")),
    }
  }

  fn disconnect(self: &Rc<Self>) {
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

  fn on_vpn_event(&self, event: VpnEvent) {
    match event {
      VpnEvent::Log { level, message } => {
        if level == LogLevel::Error {
          self.last_error.replace(Some(message.clone()));
        }
        self.append_log(level, &message);
      }
      VpnEvent::Connected => {
        if self.status.get() != VpnStatus::Disconnecting {
          self.set_status(VpnStatus::Connected);
        }
      }
      VpnEvent::Exited { success, error } => {
        self.running.set(false);
        let was_user = self.user_disconnect.replace(false);
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

  fn set_status(&self, status: VpnStatus) {
    self.status.set(status);
    let w = &self.w;
    let portal = self.settings.borrow().profile.portal.clone();

    w.status.set_icon_name(Some(status.icon_name()));
    w.status.set_title(status.label());
    let description = match (status, self.last_error.borrow().as_deref()) {
      (VpnStatus::Disconnected, Some(err)) => glib::markup_escape_text(err).to_string(),
      (VpnStatus::Disconnected, None) => "Not connected".to_owned(),
      (VpnStatus::Connecting, _) => {
        format!("Signing in to <b>{}</b>…", glib::markup_escape_text(&portal))
      }
      _ => format!("<b>{}</b>", glib::markup_escape_text(&portal)),
    };
    w.status.set_description(Some(&description));

    w.form.set_sensitive(!self.running.get());
    w.action.set_sensitive(status != VpnStatus::Disconnecting);
    let (label, add, remove) = if self.running.get() {
      let label = if status == VpnStatus::Connecting {
        "Cancel"
      } else {
        "Disconnect"
      };
      (label, "destructive-action", "suggested-action")
    } else {
      ("Connect", "suggested-action", "destructive-action")
    };
    w.action.set_label(label);
    w.action.remove_css_class(remove);
    w.action.add_css_class(add);

    w.window.set_title(Some(&format!("GlobalProtect — {}", status.label())));
    self.tray.update(status, &portal);
  }

  // ---- log ----------------------------------------------------------------

  fn clear_log(&self) {
    self.w.log_view.buffer().set_text("");
  }

  fn append_log(&self, level: LogLevel, message: &str) {
    let buffer = self.w.log_view.buffer();
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
    buffer.place_cursor(&buffer.end_iter());
    self.w.log_view.scroll_mark_onscreen(&buffer.get_insert());
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
    });
  }

  fn show_about(&self) {
    let about = adw::AboutDialog::builder()
      .application_name("GlobalProtect VPN")
      .application_icon("network-vpn")
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

fn build_widgets(app: &adw::Application, settings: &Settings) -> Widgets {
  let p = &settings.profile;

  // Portal row with a "recent portals" popover.
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
  let password = adw::PasswordEntryRow::builder().title("Password").build();
  let remember = adw::SwitchRow::builder()
    .title("Remember password")
    .subtitle("Stored in the system keyring")
    .active(p.remember_password)
    .build();

  let form = adw::PreferencesGroup::builder()
    .description("Leave the password empty for SAML / SSO portals.")
    .build();
  form.add(&portal);
  form.add(&gateway);
  form.add(&username);
  form.add(&password);
  form.add(&remember);

  let action = gtk::Button::builder()
    .label("Connect")
    .halign(gtk::Align::Center)
    .css_classes(["pill", "suggested-action"])
    .build();

  let log_view = gtk::TextView::builder()
    .editable(false)
    .cursor_visible(false)
    .monospace(true)
    .wrap_mode(gtk::WrapMode::WordChar)
    .top_margin(6)
    .bottom_margin(6)
    .left_margin(6)
    .right_margin(6)
    .build();
  let log_scroller = gtk::ScrolledWindow::builder()
    .child(&log_view)
    .min_content_height(160)
    .css_classes(["card"])
    .build();
  let log_expander = gtk::Expander::builder().label("Log").child(&log_scroller).build();

  let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
  body.append(&form);
  body.append(&action);
  body.append(&log_expander);

  let status = adw::StatusPage::builder()
    .icon_name(VpnStatus::Disconnected.icon_name())
    .title(VpnStatus::Disconnected.label())
    .child(&adw::Clamp::builder().maximum_size(480).child(&body).build())
    .build();

  let scroller = gtk::ScrolledWindow::builder()
    .hscrollbar_policy(gtk::PolicyType::Never)
    .child(&status)
    .build();

  let menu = gio::Menu::new();
  menu.append(Some("_Preferences"), Some("app.preferences"));
  menu.append(Some("_About"), Some("app.about"));
  menu.append(Some("_Quit"), Some("app.quit"));
  let menu_button = gtk::MenuButton::builder()
    .icon_name("open-menu-symbolic")
    .menu_model(&menu)
    .primary(true)
    .tooltip_text("Main Menu")
    .build();
  let header = adw::HeaderBar::new();
  header.pack_end(&menu_button);

  let toasts = adw::ToastOverlay::new();
  toasts.set_child(Some(&scroller));

  let view = adw::ToolbarView::new();
  view.add_top_bar(&header);
  view.set_content(Some(&toasts));

  let window = adw::ApplicationWindow::builder()
    .application(app)
    .title("GlobalProtect")
    .default_width(460)
    .default_height(720)
    .content(&view)
    .build();

  Widgets {
    window,
    toasts,
    status,
    form,
    portal,
    recent_list,
    recent_button,
    gateway,
    username,
    password,
    remember,
    action,
    log_view,
  }
}
