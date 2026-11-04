//! StatusNotifierItem tray icon (KDE, GNOME with AppIndicator extension, …).

use ksni::blocking::TrayMethods;

use crate::state::VpnStatus;

#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
  ShowWindow,
  Connect,
  Disconnect,
  Quit,
}

pub struct VpnTray {
  status: VpnStatus,
  failed: bool,
  portal: String,
  icon_theme_path: String,
  tx: async_channel::Sender<TrayCommand>,
}

impl VpnTray {
  fn send(&self, cmd: TrayCommand) {
    let _ = self.tx.send_blocking(cmd);
  }
}

impl ksni::Tray for VpnTray {
  fn id(&self) -> String {
    "gpgui-gtk".into()
  }

  fn title(&self) -> String {
    "GlobalProtect VPN".into()
  }

  fn icon_name(&self) -> String {
    self.status.icon_name(self.failed).into()
  }

  fn icon_theme_path(&self) -> String {
    self.icon_theme_path.clone()
  }

  fn tool_tip(&self) -> ksni::ToolTip {
    let description = if self.portal.is_empty() {
      self.status.label().to_owned()
    } else {
      format!("{} — {}", self.status.label(), self.portal)
    };
    ksni::ToolTip {
      title: "GlobalProtect VPN".into(),
      description,
      icon_name: self.status.icon_name(self.failed).into(),
      icon_pixmap: Vec::new(),
    }
  }

  fn activate(&mut self, _x: i32, _y: i32) {
    self.send(TrayCommand::ShowWindow);
  }

  fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
    use ksni::menu::*;
    let busy = self.status.is_busy();
    vec![
      StandardItem {
        label: format!("Status: {}", self.status.label()),
        enabled: false,
        ..Default::default()
      }
      .into(),
      MenuItem::Separator,
      StandardItem {
        label: "Connect".into(),
        icon_name: "network-vpn-symbolic".into(),
        visible: !self.status.is_connected() && !busy,
        activate: Box::new(|this: &mut Self| this.send(TrayCommand::Connect)),
        ..Default::default()
      }
      .into(),
      StandardItem {
        label: "Disconnect".into(),
        icon_name: "network-vpn-disabled-symbolic".into(),
        visible: self.status.is_connected() || busy,
        activate: Box::new(|this: &mut Self| this.send(TrayCommand::Disconnect)),
        ..Default::default()
      }
      .into(),
      StandardItem {
        label: "Show Window".into(),
        activate: Box::new(|this: &mut Self| this.send(TrayCommand::ShowWindow)),
        ..Default::default()
      }
      .into(),
      MenuItem::Separator,
      StandardItem {
        label: "Quit".into(),
        icon_name: "application-exit-symbolic".into(),
        activate: Box::new(|this: &mut Self| this.send(TrayCommand::Quit)),
        ..Default::default()
      }
      .into(),
    ]
  }
}

pub struct TrayHandle(Option<ksni::blocking::Handle<VpnTray>>);

impl TrayHandle {
  /// Spawns the tray; returns a no-op handle when no StatusNotifier host is available.
  pub fn spawn(tx: async_channel::Sender<TrayCommand>, icon_theme_path: Option<&std::path::Path>) -> Self {
    let tray = VpnTray {
      status: VpnStatus::Disconnected,
      failed: false,
      portal: String::new(),
      icon_theme_path: icon_theme_path.map(|p| p.display().to_string()).unwrap_or_default(),
      tx,
    };
    match tray.spawn() {
      Ok(handle) => Self(Some(handle)),
      Err(err) => {
        log::warn!("System tray unavailable: {err}");
        Self(None)
      }
    }
  }

  pub fn is_available(&self) -> bool {
    self.0.is_some()
  }

  pub fn update(&self, status: VpnStatus, failed: bool, portal: &str) {
    if let Some(handle) = &self.0 {
      let portal = portal.to_owned();
      handle.update(move |tray| {
        tray.status = status;
        tray.failed = failed;
        tray.portal = portal;
      });
    }
  }
}
