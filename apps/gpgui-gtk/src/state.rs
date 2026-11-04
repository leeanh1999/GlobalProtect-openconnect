#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnStatus {
  Disconnected,
  Connecting,
  Connected,
  Disconnecting,
}

impl VpnStatus {
  pub fn label(self) -> &'static str {
    match self {
      VpnStatus::Disconnected => "Disconnected",
      VpnStatus::Connecting => "Connecting…",
      VpnStatus::Connected => "Connected",
      VpnStatus::Disconnecting => "Disconnecting…",
    }
  }

  /// Bundled icon (see `icons.rs`); `failed` shows the error shield when disconnected.
  pub fn icon_name(self, failed: bool) -> &'static str {
    match self {
      VpnStatus::Connected => "gpgui-gtk-connected",
      VpnStatus::Connecting | VpnStatus::Disconnecting => "gpgui-gtk-connecting",
      VpnStatus::Disconnected if failed => "gpgui-gtk-error",
      VpnStatus::Disconnected => "gpgui-gtk-disconnected",
    }
  }

  pub fn is_busy(self) -> bool {
    matches!(self, VpnStatus::Connecting | VpnStatus::Disconnecting)
  }

  pub fn is_connected(self) -> bool {
    self == VpnStatus::Connected
  }
}
