#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnStatus {
  Disconnected,
  Connecting,
  Connected,
  Disconnecting,
  /// Waiting to reconnect after the connection dropped.
  Reconnecting,
}

/// Which status artwork to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusIcon {
  Connected,
  Connecting,
  Disconnected,
  Error,
}

impl VpnStatus {
  pub fn label(self) -> &'static str {
    match self {
      VpnStatus::Disconnected => "Disconnected",
      VpnStatus::Connecting => "Connecting…",
      VpnStatus::Connected => "Connected",
      VpnStatus::Disconnecting => "Disconnecting…",
      VpnStatus::Reconnecting => "Reconnecting…",
    }
  }

  /// `failed` shows the error shield when disconnected.
  pub fn icon(self, failed: bool) -> StatusIcon {
    match self {
      VpnStatus::Connected => StatusIcon::Connected,
      VpnStatus::Connecting | VpnStatus::Disconnecting | VpnStatus::Reconnecting => StatusIcon::Connecting,
      VpnStatus::Disconnected if failed => StatusIcon::Error,
      VpnStatus::Disconnected => StatusIcon::Disconnected,
    }
  }

  pub fn is_busy(self) -> bool {
    matches!(
      self,
      VpnStatus::Connecting | VpnStatus::Disconnecting | VpnStatus::Reconnecting
    )
  }

  pub fn is_connected(self) -> bool {
    self == VpnStatus::Connected
  }
}
