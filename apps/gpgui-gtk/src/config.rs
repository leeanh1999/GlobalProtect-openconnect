use std::{fs, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "io.github.gpgui_gtk.GpguiGtk";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ClientOs {
  #[default]
  Linux,
  Windows,
  Mac,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
  pub portal: String,
  pub gateway: String,
  pub username: String,
  pub remember_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub profile: Profile,
  /// Previously used portals, most recent first.
  pub recent_portals: Vec<String>,

  // Connection options forwarded to `gpclient connect`.
  pub as_gateway: bool,
  pub auto_gateway: bool,
  pub ignore_tls_errors: bool,
  pub no_dtls: bool,
  pub disable_ipv6: bool,
  pub use_default_browser: bool,
  pub hip: bool,
  pub os: ClientOs,
  pub client_version: String,
  pub mtu: u32,
  pub reconnect_timeout: u32,
  pub extra_args: String,

  // Application behaviour.
  pub gpclient_path: String,
  pub close_to_tray: bool,
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
      no_dtls: false,
      disable_ipv6: false,
      use_default_browser: false,
      hip: false,
      os: ClientOs::Linux,
      client_version: String::new(),
      mtu: 0,
      reconnect_timeout: 300,
      extra_args: String::new(),
      gpclient_path: String::new(),
      close_to_tray: true,
      start_minimized: false,
    }
  }
}

const MAX_RECENT: usize = 10;

impl Settings {
  fn path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "gpgui_gtk", "gpgui-gtk")
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

  pub fn remember_portal(&mut self, portal: &str) {
    let portal = portal.trim();
    if portal.is_empty() {
      return;
    }
    self.recent_portals.retain(|p| p != portal);
    self.recent_portals.insert(0, portal.to_owned());
    self.recent_portals.truncate(MAX_RECENT);
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
  }

  #[test]
  fn missing_fields_fall_back_to_defaults() {
    let settings: Settings = serde_json::from_str(r#"{"no_dtls":true}"#).unwrap();
    assert!(settings.no_dtls);
    assert_eq!(settings.reconnect_timeout, 300);
    assert!(settings.close_to_tray);
  }
}
