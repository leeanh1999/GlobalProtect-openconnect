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

  pub fn icon_name(self) -> &'static str {
    match self {
      VpnStatus::Connected => "network-vpn-symbolic",
      VpnStatus::Connecting | VpnStatus::Disconnecting => "network-vpn-acquiring-symbolic",
      VpnStatus::Disconnected => "network-vpn-disabled-symbolic",
    }
  }

  pub fn is_busy(self) -> bool {
    matches!(self, VpnStatus::Connecting | VpnStatus::Disconnecting)
  }

  pub fn is_connected(self) -> bool {
    self == VpnStatus::Connected
  }
}
