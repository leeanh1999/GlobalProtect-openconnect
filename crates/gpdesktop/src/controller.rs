//! Connection state machine shared by the desktop front ends.
//!
//! The controller runs as a task on a tokio runtime. A front end sends
//! [`Command`]s and renders the [`UiEvent`]s it receives through a callback;
//! all connection logic (sign-in, asking gpservice for the tunnel, auto
//! reconnect with back-off, re-authentication, stray processes) lives here.
//! Behaviour follows gpgui-gtk's window.rs.

use std::{
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpapi::service::vpn_state::VpnState;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
  LogLevel,
  auth::{self, AuthInput, AuthUi},
  config::Settings,
  procs::{self, VpnProcess},
  service::{self, ServiceCommand, ServiceEvent, ServiceHandle},
  state::{StatusIcon, VpnStatus},
};

/// What the main form holds when the user presses Connect.
#[derive(Clone, Default)]
pub struct FormInput {
  pub portal: String,
  pub gateway: String,
  pub username: String,
  pub password: String,
  pub remember_password: bool,
}

impl std::fmt::Debug for FormInput {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FormInput")
      .field("portal", &self.portal)
      .field("gateway", &self.gateway)
      .field("username", &self.username)
      .field("password", &(!self.password.is_empty()))
      .field("remember_password", &self.remember_password)
      .finish()
  }
}

#[derive(Debug)]
pub enum Command {
  /// Connect with the given form values (Connect button).
  Connect(FormInput),
  /// Connect with the saved profile and password (tray, start-up).
  ConnectSaved,
  /// Disconnect, or cancel sign-in / a pending reconnect.
  Disconnect,
  /// Menu action: list and stop VPN processes not started by us.
  StopOthers,
  SetAutoReconnect(bool),
  /// Preferences were saved (the controller keeps profile and recent portals).
  UpdateSettings(Box<Settings>),
  /// Windows: start the stopped service (UAC prompt).
  StartService,
  /// Attach to the service again (e.g. after it was restarted).
  RetryService,
  /// Leave the app. `disconnect` also stops the tunnel first.
  Quit {
    disconnect: bool,
  },
}

/// Everything a front end needs to render the status area, window title and tray.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusView {
  pub status: VpnStatus,
  pub icon: StatusIcon,
  /// "Connected", "Connecting…", …
  pub title: String,
  /// Portal/gateway, progress text or the last error.
  pub detail: String,
  /// `detail` is an error message.
  pub failed: bool,
  pub portal: String,
  pub gateway: String,
  /// Unix time the tunnel came up (Connected only).
  pub connected_at: Option<u64>,
  /// The form must not be edited (sign-in or tunnel in progress).
  pub form_locked: bool,
  /// Label of the main button: Connect / Cancel / Disconnect.
  pub action: ActionKind,
  pub action_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
  Connect,
  Cancel,
  Disconnect,
}

impl ActionKind {
  pub fn label(self) -> &'static str {
    match self {
      ActionKind::Connect => "Connect",
      ActionKind::Cancel => "Cancel",
      ActionKind::Disconnect => "Disconnect",
    }
  }
}

pub enum UiEvent {
  Status(StatusView),
  Log(LogLevel, String),
  /// Short transient message (also shown in the tray when the window is hidden).
  Toast(String),
  /// MFA/OTP prompt; reply `None` to cancel sign-in.
  AskOtp {
    message: String,
    reply: oneshot::Sender<Option<String>>,
  },
  /// Ask before stopping other VPN processes; reply `true` to stop them.
  ConfirmStopOthers {
    processes: Vec<VpnProcess>,
    then_connect: bool,
    reply: oneshot::Sender<bool>,
  },
  /// The service is installed but stopped; offer [`Command::StartService`].
  ServiceNotRunning(String),
  /// Settings changed inside the controller (recent portals, auto reconnect).
  Settings(Box<Settings>),
  /// The quit sequence finished; the front end should exit now.
  Exit,
}

#[derive(Clone)]
pub struct ControllerHandle {
  tx: mpsc::UnboundedSender<Command>,
}

impl ControllerHandle {
  pub fn send(&self, command: Command) {
    let _ = self.tx.send(command);
  }
}

type UiSink = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Starts the controller on `rt`. `ui` is called from the runtime's threads.
pub fn spawn(
  rt: &tokio::runtime::Handle,
  settings: Settings,
  ui: impl Fn(UiEvent) + Send + Sync + 'static,
) -> ControllerHandle {
  let (tx, rx) = mpsc::unbounded_channel();
  let (internal_tx, internal_rx) = mpsc::unbounded_channel();
  let controller = Controller {
    ui: Arc::new(ui),
    rt: rt.clone(),
    settings,
    status: VpnStatus::Disconnected,
    service: None,
    service_rx: None,
    service_session: None,
    auth_cancel: None,
    tunnel_requested: false,
    was_connected: false,
    user_disconnect: false,
    reconnecting: false,
    reconnect_failures: 0,
    reconnect_remaining: 0,
    reconnect_timer: false,
    restart_of: None,
    last_error: None,
    gateway_name: String::new(),
    connected_at: None,
    last_input: None,
    quitting: None,
    internal_tx,
  };
  rt.spawn(controller.run(rx, internal_rx));
  ControllerHandle { tx }
}

/// Results of work the controller spawned.
enum Internal {
  Log(LogLevel, String),
  Accepted(String),
  AttemptFailed {
    error: String,
    cancelled: bool,
  },
  StopOthersAnswer {
    pids: Vec<u32>,
    then_connect: bool,
    accepted: bool,
  },
  StopOthersDone {
    result: Result<(), String>,
    then_connect: bool,
  },
  OthersFound {
    processes: Vec<VpnProcess>,
    then_connect: bool,
  },
  ServiceStart(Result<(), String>),
}

struct Controller {
  ui: UiSink,
  rt: tokio::runtime::Handle,
  settings: Settings,
  status: VpnStatus,
  service: Option<ServiceHandle>,
  service_rx: Option<mpsc::UnboundedReceiver<ServiceEvent>>,
  /// Our gpservice session (to match `ReconnectRequired.owner`).
  service_session: Option<Uuid>,
  auth_cancel: Option<CancellationToken>,
  /// gpservice accepted (or reported) a tunnel we are responsible for.
  tunnel_requested: bool,
  was_connected: bool,
  user_disconnect: bool,
  reconnecting: bool,
  reconnect_failures: u32,
  reconnect_remaining: u32,
  reconnect_timer: bool,
  restart_of: Option<u64>,
  last_error: Option<String>,
  gateway_name: String,
  connected_at: Option<u64>,
  /// Inputs of the last attempt, reused by automatic reconnects.
  last_input: Option<FormInput>,
  /// Quit requested; `Some(deadline)` while waiting for the tunnel to stop.
  quitting: Option<tokio::time::Instant>,
  internal_tx: mpsc::UnboundedSender<Internal>,
}

async fn recv_service(rx: &mut Option<mpsc::UnboundedReceiver<ServiceEvent>>) -> Option<ServiceEvent> {
  match rx {
    Some(rx) => rx.recv().await,
    None => std::future::pending().await,
  }
}

impl Controller {
  fn emit(&self, event: UiEvent) {
    (self.ui)(event);
  }

  fn log(&self, level: LogLevel, message: impl Into<String>) {
    let message = message.into();
    match level {
      LogLevel::Error => log::error!("{message}"),
      LogLevel::Warn => log::warn!("{message}"),
      LogLevel::Info => log::info!("{message}"),
      LogLevel::Debug => log::debug!("{message}"),
    }
    self.emit(UiEvent::Log(level, message));
  }

  fn toast(&self, message: impl Into<String>) {
    self.emit(UiEvent::Toast(message.into()));
  }

  fn save_settings(&self) {
    if let Err(err) = self.settings.save() {
      self.log(LogLevel::Warn, format!("Cannot save settings: {err:#}"));
    }
  }

  async fn run(
    mut self,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut internal: mpsc::UnboundedReceiver<Internal>,
  ) {
    self.set_status(VpnStatus::Disconnected);
    // On Windows the service is always running: attach at start-up to show a
    // tunnel that outlived a previous instance of the app.
    if cfg!(windows) {
      self.ensure_service();
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
      tokio::select! {
        command = commands.recv() => {
          let Some(command) = command else { break };
          self.on_command(command);
        }
        event = recv_service(&mut self.service_rx) => match event {
          Some(event) => self.on_service_event(event),
          None => self.service_rx = None,
        },
        Some(event) = internal.recv() => self.on_internal(event),
        _ = tick.tick() => self.on_tick(),
      }
      if self.quit_finished() {
        break;
      }
    }
    if let Some(cancel) = &self.auth_cancel {
      cancel.cancel();
    }
    // Dropping the handle releases the service session (Linux: stops gpservice).
    self.service = None;
    self.emit(UiEvent::Exit);
  }

  // ---- commands -------------------------------------------------------------

  fn on_command(&mut self, command: Command) {
    match command {
      Command::Connect(input) => self.connect(input),
      Command::ConnectSaved => {
        let input = self.saved_input();
        self.connect(input);
      }
      Command::Disconnect => self.disconnect(),
      Command::StopOthers => self.find_others(false),
      Command::SetAutoReconnect(enabled) => self.set_auto_reconnect(enabled),
      Command::UpdateSettings(updated) => {
        let profile = std::mem::take(&mut self.settings.profile);
        let recent = std::mem::take(&mut self.settings.recent_portals);
        self.settings = Settings {
          profile,
          recent_portals: recent,
          ..*updated
        };
        self.save_settings();
        let enabled = self.settings.auto_reconnect;
        self.set_auto_reconnect(enabled);
      }
      Command::StartService => {
        #[cfg(windows)]
        {
          let tx = self.internal_tx.clone();
          self.log(LogLevel::Info, "Starting the VPN service…");
          self.rt.spawn(async move {
            let result = service::windows::request_start().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::ServiceStart(result));
          });
        }
      }
      Command::RetryService => {
        self.ensure_service();
      }
      Command::Quit { disconnect } => self.quit(disconnect),
    }
  }

  /// Form values from the saved profile (and saved password on Windows).
  fn saved_input(&self) -> FormInput {
    let profile = &self.settings.profile;
    #[allow(unused_mut)]
    let mut input = FormInput {
      portal: profile.portal.clone(),
      gateway: profile.gateway.clone(),
      username: profile.username.clone(),
      password: String::new(),
      remember_password: profile.remember_password,
    };
    #[cfg(windows)]
    if profile.remember_password && !profile.username.trim().is_empty() {
      match crate::secrets::load_password(&profile.portal, &profile.username) {
        Ok(Some(password)) => input.password = password.to_string(),
        Ok(None) => {}
        Err(err) => self.log(LogLevel::Warn, format!("{err:#}")),
      }
    }
    input
  }

  fn is_active(&self) -> bool {
    self.auth_cancel.is_some() || self.tunnel_requested || self.status.is_busy() || self.status.is_connected()
  }

  fn connect(&mut self, input: FormInput) {
    if self.is_active() || self.quitting.is_some() {
      return;
    }
    if input.portal.trim().is_empty() {
      self.toast("Enter a portal address");
      return;
    }
    self.cancel_reconnect();
    self.reconnect_failures = 0;
    self.restart_of = None;
    self.last_input = Some(input);
    self.find_others(true);
  }

  /// Looks for stray VPN processes; connects right away when there are none.
  fn find_others(&mut self, then_connect: bool) {
    let exclude: Vec<u32> = self.service.as_ref().and_then(|s| s.pid).into_iter().collect();
    let tx = self.internal_tx.clone();
    self.rt.spawn(async move {
      let processes = tokio::task::spawn_blocking(move || procs::find_vpn_processes(&exclude))
        .await
        .unwrap_or_default();
      let _ = tx.send(Internal::OthersFound {
        processes,
        then_connect,
      });
    });
  }

  fn persist_form(&mut self, input: &FormInput) {
    let profile = &mut self.settings.profile;
    profile.portal = input.portal.trim().to_owned();
    profile.gateway = input.gateway.trim().to_owned();
    profile.username = input.username.trim().to_owned();
    profile.remember_password = input.remember_password;
    let portal = profile.portal.clone();
    self.settings.remember_portal(&portal);
    #[cfg(windows)]
    {
      let username = input.username.trim();
      if !username.is_empty() {
        let result = if input.remember_password && !input.password.is_empty() {
          crate::secrets::store_password(&portal, username, &input.password)
        } else if !input.remember_password {
          crate::secrets::delete_password(&portal, username)
        } else {
          Ok(())
        };
        if let Err(err) = result {
          self.log(LogLevel::Warn, format!("Keyring update failed: {err:#}"));
        }
      }
    }
    self.save_settings();
    self.emit(UiEvent::Settings(Box::new(self.settings.clone())));
  }

  /// Starts (Linux) or attaches to (Windows) gpservice.
  fn ensure_service(&mut self) -> Option<mpsc::UnboundedSender<ServiceCommand>> {
    if let Some(handle) = self.service.as_ref().filter(|s| s.is_alive()) {
      return Some(handle.sender());
    }
    let (tx, rx) = mpsc::unbounded_channel();
    match service::start(&self.rt, &self.settings.gpservice_path, tx) {
      Ok(handle) => {
        let sender = handle.sender();
        self.service = Some(handle);
        self.service_rx = Some(rx);
        Some(sender)
      }
      Err(err) => {
        #[cfg(windows)]
        if err.downcast_ref::<service::windows::ServiceNotRunning>().is_some() {
          self.emit(UiEvent::ServiceNotRunning(format!("{err:#}")));
          return None;
        }
        self.log(LogLevel::Error, format!("{err:#}"));
        self.toast(format!("{err:#}"));
        None
      }
    }
  }

  /// Signs in (in the user session) and asks gpservice to open the tunnel.
  fn start_connect(&mut self) {
    if self.auth_cancel.is_some() || self.tunnel_requested {
      return;
    }
    let Some(input) = self.last_input.clone() else { return };
    let portal = input.portal.trim().to_owned();
    let Some(service) = self.ensure_service() else {
      self.reconnecting = false;
      self.set_status(VpnStatus::Disconnected);
      return;
    };

    self.persist_form(&input);
    let opt = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    let auth_input = AuthInput {
      portal: portal.clone(),
      gateway: opt(&input.gateway),
      username: opt(&input.username),
      password: Some(input.password.clone()).filter(|p| !p.is_empty()),
      settings: self.settings.clone(),
    };

    let verb = if self.reconnecting {
      "Reconnecting to"
    } else {
      "Connecting to"
    };
    self.log(LogLevel::Info, format!("──── {verb} {portal} ────"));
    self.was_connected = false;
    self.user_disconnect = false;
    if !self.reconnecting {
      self.last_error = None;
    }
    self.gateway_name.clear();
    let cancel = CancellationToken::new();
    self.auth_cancel = Some(cancel.clone());
    self.set_status(VpnStatus::Connecting);

    let restart_of = self.restart_of.take();
    let tx = self.internal_tx.clone();
    let ui = self.ui.clone();
    self.rt.spawn(async move {
      let log_tx = tx.clone();
      let otp_ui = ui.clone();
      let auth_ui = AuthUi {
        log: Box::new(move |msg| {
          let _ = log_tx.send(Internal::Log(LogLevel::Info, msg));
        }),
        ask_otp: Box::new(move |message| {
          let (reply, rx) = oneshot::channel();
          otp_ui(UiEvent::AskOtp { message, reply });
          rx
        }),
      };
      let outcome = async {
        let authenticated = auth::authenticate(&auth_input, &auth_ui, &cancel).await?;
        let gateway = authenticated.gateway_name.clone();
        let request = authenticated.request.with_restart_of(restart_of);
        let (reply, accepted) = oneshot::channel();
        let _ = tx.send(Internal::Log(
          LogLevel::Info,
          format!("Asking the VPN service to connect to {gateway}"),
        ));
        let result = if service.send(ServiceCommand::Connect(Box::new(request), reply)).is_ok() {
          accepted
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("The VPN service stopped")))
        } else {
          Err(anyhow::anyhow!("The VPN service is not running"))
        };
        match result {
          // The service now owns the gateway sessions (and logs them out itself).
          Ok(()) => {
            authenticated.sessions.relinquish();
            Ok(gateway)
          }
          Err(err) => {
            authenticated.sessions.logout().await;
            Err(err.context("The VPN service refused the connection"))
          }
        }
      }
      .await;
      let event = match outcome {
        Ok(gateway) => Internal::Accepted(gateway),
        Err(err) => Internal::AttemptFailed {
          cancelled: auth::is_cancelled(&err) || cancel.is_cancelled(),
          error: format!("{err:#}"),
        },
      };
      let _ = tx.send(event);
    });
  }

  fn disconnect(&mut self) {
    match self.status {
      VpnStatus::Reconnecting => {
        self.cancel_reconnect();
        self.last_error = None;
        self.set_status(VpnStatus::Disconnected);
        return;
      }
      VpnStatus::Disconnecting => return,
      _ => {}
    }
    self.user_disconnect = true;
    if let Some(cancel) = &self.auth_cancel {
      self.log(LogLevel::Info, "Sign-in cancelled");
      cancel.cancel();
    }
    if self.tunnel_requested || self.status == VpnStatus::Connected {
      let sent = self
        .service
        .as_ref()
        .is_some_and(|s| s.send(ServiceCommand::Disconnect));
      if sent {
        self.set_status(VpnStatus::Disconnecting);
      } else {
        self.tunnel_requested = false;
        self.set_status(VpnStatus::Disconnected);
      }
    }
  }

  fn quit(&mut self, disconnect: bool) {
    if self.quitting.is_some() {
      return;
    }
    self.cancel_reconnect();
    if let Some(cancel) = &self.auth_cancel {
      cancel.cancel();
    }
    let tunnel = self.tunnel_requested || self.status.is_connected() || self.status == VpnStatus::Disconnecting;
    if disconnect && tunnel {
      self.disconnect();
      self.quitting = Some(tokio::time::Instant::now() + Duration::from_secs(10));
    } else {
      self.quitting = Some(tokio::time::Instant::now());
    }
  }

  fn quit_finished(&self) -> bool {
    match self.quitting {
      None => false,
      Some(deadline) => {
        tokio::time::Instant::now() >= deadline
          || (self.auth_cancel.is_none()
            && !self.tunnel_requested
            && !self.status.is_busy()
            && !self.status.is_connected())
      }
    }
  }

  // ---- events ---------------------------------------------------------------

  fn on_internal(&mut self, event: Internal) {
    match event {
      Internal::Log(level, message) => self.log(level, message),
      Internal::Accepted(gateway) => {
        self.auth_cancel = None;
        self.tunnel_requested = true;
        self.log(
          LogLevel::Info,
          format!("The VPN service accepted the connection to {gateway}"),
        );
        self.gateway_name = gateway;
        if self.status != VpnStatus::Connected {
          self.set_status(VpnStatus::Connecting);
        }
      }
      Internal::AttemptFailed { error, cancelled } => {
        self.auth_cancel = None;
        let was_user = std::mem::take(&mut self.user_disconnect) || cancelled;
        if was_user {
          self.log(LogLevel::Info, "Sign-in cancelled");
          self.reconnecting = false;
          self.last_error = None;
          self.set_status(VpnStatus::Disconnected);
        } else {
          self.log(LogLevel::Error, error.clone());
          self.attempt_failed(error);
        }
      }
      Internal::OthersFound {
        processes,
        then_connect,
      } => {
        if processes.is_empty() {
          if then_connect {
            self.start_connect();
          } else {
            self.toast("No other VPN processes are running");
          }
          return;
        }
        let (reply, answer) = oneshot::channel();
        let pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
        self.emit(UiEvent::ConfirmStopOthers {
          processes,
          then_connect,
          reply,
        });
        let tx = self.internal_tx.clone();
        self.rt.spawn(async move {
          let accepted = answer.await.unwrap_or(false);
          let _ = tx.send(Internal::StopOthersAnswer {
            pids,
            then_connect,
            accepted,
          });
        });
      }
      Internal::StopOthersAnswer {
        pids,
        then_connect,
        accepted,
      } => {
        if !accepted {
          return;
        }
        let tx = self.internal_tx.clone();
        self.rt.spawn(async move {
          let result = tokio::task::spawn_blocking(move || procs::kill_processes(&pids))
            .await
            .map_err(|_| "Stopping processes panicked".to_owned())
            .and_then(|r| r.map_err(|e| format!("Cannot stop processes: {e:#}")));
          let _ = tx.send(Internal::StopOthersDone { result, then_connect });
        });
      }
      Internal::StopOthersDone { result, then_connect } => match result {
        Ok(()) => {
          let exclude: Vec<u32> = self.service.as_ref().and_then(|s| s.pid).into_iter().collect();
          let left = procs::find_vpn_processes(&exclude);
          if left.is_empty() {
            self.toast("Stopped other VPN processes");
            if then_connect {
              self.start_connect();
            }
          } else {
            self.toast(format!("{} process(es) are still running", left.len()));
          }
        }
        Err(err) => self.toast(err),
      },
      Internal::ServiceStart(result) => match result {
        Ok(()) => {
          self.log(LogLevel::Info, "The VPN service is running");
          self.ensure_service();
        }
        Err(err) => {
          self.log(LogLevel::Error, err.clone());
          self.toast(err);
        }
      },
    }
  }

  fn on_service_event(&mut self, event: ServiceEvent) {
    match event {
      ServiceEvent::Log(level, message) => self.log(level, message),
      ServiceEvent::Ready { session_id, state } => {
        self.service_session = Some(session_id);
        self.log(LogLevel::Info, "Attached to the VPN service");
        self.apply_vpn_state(state);
      }
      ServiceEvent::State(state) => self.apply_vpn_state(state),
      ServiceEvent::Stopped(reason) => {
        self.log(LogLevel::Warn, format!("VPN service session ended: {reason}"));
        self.service = None;
        self.service_rx = None;
        self.service_session = None;
        if let Some(cancel) = &self.auth_cancel {
          cancel.cancel();
        }
        let was_active = std::mem::take(&mut self.tunnel_requested) || self.status == VpnStatus::Connected;
        if was_active || self.status == VpnStatus::Connecting {
          self.tunnel_ended(Some(reason));
        } else if self.status.is_busy() {
          self.set_status(VpnStatus::Disconnected);
        }
      }
    }
  }

  fn apply_vpn_state(&mut self, state: VpnState) {
    match state {
      VpnState::Connecting(info) => {
        if self.tunnel_requested {
          self.gateway_name = info.gateway().name().to_owned();
          self.set_status(VpnStatus::Connecting);
        }
      }
      VpnState::Connected(connected) => {
        self.tunnel_requested = true;
        self.was_connected = true;
        self.reconnecting = false;
        self.reconnect_failures = 0;
        self.last_error = None;
        self.gateway_name = connected.info().gateway().name().to_owned();
        if self.settings.profile.portal.is_empty() {
          self.settings.profile.portal = connected.info().portal().to_owned();
        }
        self.connected_at = Some(u64::from(connected.connected_at()));
        if self.status != VpnStatus::Connected {
          self.log(
            LogLevel::Info,
            format!("Connected to {}", connected.info().gateway().name()),
          );
        }
        self.set_status(VpnStatus::Connected);
      }
      VpnState::Disconnecting => {
        if self.tunnel_requested {
          self.set_status(VpnStatus::Disconnecting);
        }
      }
      VpnState::Disconnected => {
        if std::mem::take(&mut self.tunnel_requested) {
          self.tunnel_ended(None);
        }
      }
      VpnState::Failed(message) => {
        if std::mem::take(&mut self.tunnel_requested) {
          self.tunnel_ended(Some(message));
        }
      }
      VpnState::ReconnectRequired(request) => {
        // gpservice lost the tunnel and needs fresh credentials to resume it.
        self.tunnel_requested = false;
        self.log(LogLevel::Warn, "The VPN service requests re-authentication");
        if self.settings.auto_reconnect && !self.user_disconnect && self.quitting.is_none() {
          // Only the session that owns the attempt may resume it.
          self.restart_of = (self.service_session == Some(request.owner)).then_some(request.attempt);
          if self.last_input.is_none() {
            self.last_input = Some(self.saved_input());
          }
          self.was_connected = false;
          self.reconnecting = true;
          self.start_connect();
        } else {
          self.tunnel_ended(Some("The connection needs to be re-authenticated".into()));
        }
      }
    }
  }

  /// The tunnel we requested is gone (user disconnect, drop, or failure).
  fn tunnel_ended(&mut self, error: Option<String>) {
    let was_user = std::mem::take(&mut self.user_disconnect);
    let dropped = std::mem::take(&mut self.was_connected);
    self.connected_at = None;
    if was_user || self.quitting.is_some() {
      self.reconnecting = false;
      self.last_error = None;
      self.set_status(VpnStatus::Disconnected);
      return;
    }
    let error = error.unwrap_or_else(|| "Connection ended unexpectedly".into());
    self.log(LogLevel::Warn, error.clone());
    if dropped {
      // A dropped session starts a fresh reconnect series.
      self.reconnect_failures = 0;
      self.last_error = Some(error.clone());
      if self.settings.may_reconnect(0) {
        if self.last_input.is_none() {
          self.last_input = Some(self.saved_input());
        }
        self.schedule_reconnect();
        return;
      }
      self.toast(error);
      self.set_status(VpnStatus::Disconnected);
    } else {
      self.attempt_failed(error);
    }
  }

  /// An attempt that never reached Connected failed.
  fn attempt_failed(&mut self, error: String) {
    self.last_error = Some(error.clone());
    if self.reconnecting {
      let failures = self.reconnect_failures + 1;
      self.reconnect_failures = failures;
      if self.settings.may_reconnect(failures) {
        self.schedule_reconnect();
        return;
      }
      self.reconnecting = false;
      let msg = format!("Gave up reconnecting after {failures} attempts: {error}");
      self.log(LogLevel::Error, msg.clone());
      self.last_error = Some(msg.clone());
      self.toast(msg);
    } else {
      self.toast(error);
    }
    self.set_status(VpnStatus::Disconnected);
  }

  // ---- auto reconnect -------------------------------------------------------

  fn set_auto_reconnect(&mut self, enabled: bool) {
    if self.settings.auto_reconnect != enabled {
      self.settings.auto_reconnect = enabled;
      self.save_settings();
    }
    self.emit(UiEvent::Settings(Box::new(self.settings.clone())));
    if !enabled && self.status == VpnStatus::Reconnecting {
      self.cancel_reconnect();
      self.set_status(VpnStatus::Disconnected);
    }
  }

  /// Waits the back-off delay (with a visible countdown), then reconnects.
  fn schedule_reconnect(&mut self) {
    self.cancel_reconnect();
    let attempt = self.reconnect_failures + 1;
    let delay = self.settings.reconnect_backoff(attempt);
    self.log(
      LogLevel::Warn,
      format!("Connection lost — reconnect attempt {attempt} in {delay}s"),
    );
    self.reconnect_remaining = delay;
    self.reconnect_timer = true;
    self.set_status(VpnStatus::Reconnecting);
  }

  fn cancel_reconnect(&mut self) {
    self.reconnect_timer = false;
    self.reconnecting = false;
  }

  fn on_tick(&mut self) {
    if !self.reconnect_timer {
      return;
    }
    self.reconnect_remaining = self.reconnect_remaining.saturating_sub(1);
    if self.reconnect_remaining > 0 {
      self.set_status(VpnStatus::Reconnecting);
      return;
    }
    self.reconnect_timer = false;
    self.reconnecting = true;
    self.start_connect();
    if self.auth_cancel.is_none() {
      // Could not even start the attempt (service unavailable): stop here.
      self.reconnecting = false;
      self.set_status(VpnStatus::Disconnected);
    }
  }

  // ---- status ---------------------------------------------------------------

  fn set_status(&mut self, status: VpnStatus) {
    self.status = status;
    let view = self.status_view();
    self.emit(UiEvent::Status(view));
  }

  fn status_view(&self) -> StatusView {
    let status = self.status;
    let portal = self.settings.profile.portal.clone();
    let error = self.last_error.clone();
    let failed = error.is_some();
    let attempt = self.reconnect_failures + 1;
    let detail = match status {
      VpnStatus::Disconnected => error.unwrap_or_else(|| "Not connected".to_owned()),
      VpnStatus::Connecting if self.reconnecting => format!("Reconnecting to {portal} (attempt {attempt})…"),
      VpnStatus::Connecting if self.auth_cancel.is_some() => format!("Signing in to {portal}…"),
      VpnStatus::Connecting => format!("Connecting to {}…", self.gateway_name),
      VpnStatus::Reconnecting => format!("Retrying {portal} in {}s (attempt {attempt})", self.reconnect_remaining),
      VpnStatus::Connected | VpnStatus::Disconnecting => {
        if self.gateway_name.is_empty() {
          portal.clone()
        } else {
          format!("{portal} · {}", self.gateway_name)
        }
      }
    };
    let active = self.is_active() || status == VpnStatus::Connecting;
    let action = match status {
      _ if !active => ActionKind::Connect,
      VpnStatus::Connecting | VpnStatus::Reconnecting => ActionKind::Cancel,
      _ => ActionKind::Disconnect,
    };
    StatusView {
      status,
      icon: status.icon(failed),
      title: status.label().to_owned(),
      detail,
      failed: failed && status == VpnStatus::Disconnected,
      portal,
      gateway: self.gateway_name.clone(),
      connected_at: (status == VpnStatus::Connected).then_some(self.connected_at).flatten(),
      form_locked: active,
      action,
      action_enabled: status != VpnStatus::Disconnecting,
    }
  }
}

/// Seconds since the Unix epoch (for "Active for hh:mm:ss").
pub fn now_unix() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
}

/// `hh:mm:ss` since `since` (Unix seconds).
pub fn format_elapsed(since: u64, now: u64) -> String {
  let secs = now.saturating_sub(since);
  format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn elapsed_is_formatted_as_hours_minutes_seconds() {
    assert_eq!(format_elapsed(100, 100), "00:00:00");
    assert_eq!(format_elapsed(0, 260), "00:04:20");
    assert_eq!(format_elapsed(0, 3 * 3600 + 61), "03:01:01");
    assert_eq!(format_elapsed(500, 100), "00:00:00");
  }

  #[test]
  fn form_input_debug_hides_the_password() {
    let input = FormInput {
      portal: "vpn.example.com".into(),
      password: "hunter2".into(),
      ..Default::default()
    };
    assert!(!format!("{input:?}").contains("hunter2"));
  }

  fn collect() -> (
    Arc<std::sync::Mutex<Vec<StatusView>>>,
    impl Fn(UiEvent) + Send + Sync + 'static,
  ) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    (seen, move |event| {
      if let UiEvent::Status(view) = event {
        sink.lock().unwrap().push(view);
      }
    })
  }

  #[tokio::test]
  async fn empty_portal_is_rejected_without_connecting() {
    let (seen, ui) = collect();
    let handle = spawn(&tokio::runtime::Handle::current(), Settings::default(), ui);
    handle.send(Command::Connect(FormInput::default()));
    handle.send(Command::Quit { disconnect: false });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|v| v.status == VpnStatus::Disconnected));
    assert!(seen.iter().all(|v| v.action == ActionKind::Connect));
  }
}
