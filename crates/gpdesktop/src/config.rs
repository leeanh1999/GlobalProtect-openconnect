use std::{fs, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Product name shown to the user and used for per-user storage.
pub const APP_NAME: &str = "GlobalProtect VPN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientOs {
  Linux,
  Windows,
  Mac,
}

impl Default for ClientOs {
  fn default() -> Self {
    if cfg!(windows) {
      ClientOs::Windows
    } else if cfg!(target_os = "macos") {
      ClientOs::Mac
    } else {
      ClientOs::Linux
    }
  }
}

impl ClientOs {
  pub const ALL: [ClientOs; 3] = [ClientOs::Linux, ClientOs::Windows, ClientOs::Mac];

  /// Value accepted by `gpclient connect --os`.
  pub fn as_arg(self) -> &'static str {
    match self {
      ClientOs::Linux => "Linux",
      ClientOs::Windows => "Windows",
      ClientOs::Mac => "Mac",
    }
  }

  pub fn index(self) -> u32 {
    Self::ALL.iter().position(|os| *os == self).unwrap_or(0) as u32
  }

  pub fn from_index(index: u32) -> Self {
    Self::ALL.get(index as usize).copied().unwrap_or_default()
  }
}

/// Last values typed into the main form.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
  pub portal: String,
  pub gateway: String,
  pub username: String,
  pub remember_password: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub profile: Profile,
  /// Previously used portals, most recent first.
  pub recent_portals: Vec<String>,

  // Connection options (same meaning as `gpclient connect` options).
  pub as_gateway: bool,
  pub auto_gateway: bool,
  pub ignore_tls_errors: bool,
  /// OpenSSL legacy renegotiation (Linux/BSD only; Windows uses SChannel).
  pub fix_openssl: bool,
  pub no_dtls: bool,
  pub disable_ipv6: bool,
  pub use_default_browser: bool,
  pub hip: bool,
  pub os: ClientOs,
  pub client_version: String,
  pub mtu: u32,
  pub reconnect_timeout: u32,

  // Application behaviour.
  /// Absolute path of gpservice; empty = auto-detect (Linux/BSD only).
  pub gpservice_path: String,
  pub close_to_tray: bool,
  /// Reconnect automatically when an established connection drops.
  pub auto_reconnect: bool,
  /// First retry delay in seconds; later retries back off exponentially.
  pub reconnect_delay: u32,
  /// Give up after this many failed attempts in a row (0 = never give up).
  pub reconnect_max_attempts: u32,
  pub start_minimized: bool,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      profile: Profile::default(),
      recent_portals: Vec::new(),
      as_gateway: false,
      auto_gateway: true,
      ignore_tls_errors: false,
      fix_openssl: false,
      no_dtls: false,
      disable_ipv6: false,
      use_default_browser: false,
      hip: false,
      os: ClientOs::default(),
      client_version: String::new(),
      mtu: 0,
      reconnect_timeout: 300,
      gpservice_path: String::new(),
      close_to_tray: true,
      auto_reconnect: true,
      reconnect_delay: 5,
      reconnect_max_attempts: 0,
      start_minimized: false,
    }
  }
}

pub const MAX_RECENT: usize = 10;
const MAX_RECONNECT_DELAY: u32 = 300;

impl Settings {
  /// `%APPDATA%\GlobalProtect VPN\config\settings.json` on Windows,
  /// `~/.config/gpgui-gtk/settings.json` elsewhere (compatible with gpgui-gtk).
  pub fn path() -> anyhow::Result<PathBuf> {
    let dirs = if cfg!(windows) {
      directories::ProjectDirs::from("", "", APP_NAME)
    } else {
      directories::ProjectDirs::from("io.github", "gpgui_gtk", "gpgui-gtk")
    }
    .context("Cannot determine the configuration directory")?;
    Ok(dirs.config_dir().join("settings.json"))
  }

  pub fn load() -> Self {
    let result = Self::path().and_then(|path| {
      let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
      Ok(serde_json::from_slice::<Settings>(&data)?)
    });
    match result {
      Ok(settings) => settings,
      Err(err) => {
        log::info!("Using default settings: {err:#}");
        Self::default()
      }
    }
  }

  pub fn save(&self) -> anyhow::Result<()> {
    let path = Self::path()?;
    if let Some(dir) = path.parent() {
      fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
    fs::rename(tmp, path)?;
    Ok(())
  }

  /// Delay before reconnect attempt `attempt` (1-based): base, 2×, 4×, … capped at 5 min.
  pub fn reconnect_backoff(&self, attempt: u32) -> u32 {
    let base = self.reconnect_delay.max(1);
    let factor = 1u32 << attempt.saturating_sub(1).min(8);
    base.saturating_mul(factor).min(MAX_RECONNECT_DELAY)
  }

  /// Whether another reconnect attempt is allowed after `failed` attempts in a row.
  pub fn may_reconnect(&self, failed: u32) -> bool {
    self.auto_reconnect && (self.reconnect_max_attempts == 0 || failed < self.reconnect_max_attempts)
  }

  pub fn remember_portal(&mut self, portal: &str) {
    let portal = portal.trim();
    if portal.is_empty() {
      return;
    }
    self.recent_portals.retain(|p| p != portal);
    self.recent_portals.insert(0, portal.to_owned());
    self.recent_portals.truncate(MAX_RECENT);
  }

  pub fn forget_portal(&mut self, portal: &str) {
    self.recent_portals.retain(|p| p != portal.trim());
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recent_portals_are_deduplicated_and_bounded() {
    let mut settings = Settings::default();
    for i in 0..15 {
      settings.remember_portal(&format!("p{i}"));
    }
    settings.remember_portal("p3");
    assert_eq!(settings.recent_portals.len(), MAX_RECENT);
    assert_eq!(settings.recent_portals[0], "p3");
    assert_eq!(settings.recent_portals.iter().filter(|p| *p == "p3").count(), 1);
    settings.forget_portal(" p3 ");
    assert!(!settings.recent_portals.contains(&"p3".to_owned()));
  }

  #[test]
  fn reconnect_backoff_doubles_and_caps() {
    let s = Settings {
      reconnect_delay: 5,
      ..Default::default()
    };
    let delays: Vec<u32> = (1..=9).map(|a| s.reconnect_backoff(a)).collect();
    assert_eq!(delays, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    let zero = Settings {
      reconnect_delay: 0,
      ..Default::default()
    };
    assert_eq!(zero.reconnect_backoff(1), 1);
  }

  #[test]
  fn reconnect_attempt_limit() {
    let mut s = Settings::default();
    assert!(s.may_reconnect(1000), "0 means unlimited");
    s.reconnect_max_attempts = 3;
    assert!(s.may_reconnect(2));
    assert!(!s.may_reconnect(3));
    s.auto_reconnect = false;
    assert!(!s.may_reconnect(0));
  }

  #[test]
  fn missing_fields_fall_back_to_defaults() {
    let settings: Settings = serde_json::from_str(r#"{"no_dtls":true}"#).unwrap();
    assert!(settings.no_dtls);
    assert_eq!(settings.reconnect_timeout, 300);
    assert!(settings.close_to_tray);
    assert_eq!(settings.os, ClientOs::default());
  }

  #[test]
  fn client_os_index_round_trips() {
    for os in ClientOs::ALL {
      assert_eq!(ClientOs::from_index(os.index()), os);
    }
    assert_eq!(ClientOs::from_index(99), ClientOs::default());
  }
}
