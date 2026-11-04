//! Drives `gpclient` (running as root through `pkexec`) and turns its JSON log
//! stream into [`VpnEvent`]s for the UI.
//!
//! Using the CLI keeps the GUI independent from gpservice's private transport
//! protocol, and reuses the whole upstream auth flow (password, SAML through
//! `gpauth`, certificates, HIP).

use std::{
  env,
  io::{BufRead, BufReader, Read, Write},
  path::{Path, PathBuf},
  process::{Child, Command, Stdio},
  thread,
};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::config::Settings;

/// Log line from gpclient that marks the tunnel as up (crates/openconnect/src/vpn.rs).
const CONNECTED_MARKER: &str = "Connected to VPN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
  Error,
  Warn,
  Info,
  Debug,
}

#[derive(Debug, Clone)]
pub enum VpnEvent {
  Log { level: LogLevel, message: String },
  Connected,
  Exited { success: bool, error: Option<String> },
}

#[derive(Debug, Clone)]
pub struct ConnectRequest {
  pub portal: String,
  pub gateway: Option<String>,
  pub username: Option<String>,
  pub password: Option<String>,
}

#[derive(Deserialize)]
struct JsonRecord {
  level: String,
  message: String,
}

pub fn resolve_gpclient(settings: &Settings) -> anyhow::Result<PathBuf> {
  let configured = settings.gpclient_path.trim();
  if !configured.is_empty() {
    let path = PathBuf::from(configured);
    if !path.is_absolute() {
      bail!("gpclient path must be absolute: {configured}");
    }
    if !path.is_file() {
      bail!("gpclient not found at {configured}");
    }
    return Ok(path);
  }
  find_in_path("gpclient")
    .or_else(|| {
      ["/usr/bin/gpclient", "/usr/local/bin/gpclient"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    })
    .context("gpclient was not found. Install GlobalProtect-openconnect or set its path in Preferences")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
  env::var_os("PATH").and_then(|paths| {
    env::split_paths(&paths)
      .map(|dir| dir.join(name))
      .find(|candidate| candidate.is_file())
  })
}

/// Arguments for `gpclient` (excluding the binary itself).
pub fn connect_args(settings: &Settings, req: &ConnectRequest) -> Vec<String> {
  let mut args: Vec<String> = Vec::new();
  if settings.ignore_tls_errors {
    args.push("--ignore-tls-errors".into());
  }
  args.extend(["--log-format".into(), "json".into(), "connect".into()]);

  if settings.as_gateway {
    args.push("--as-gateway".into());
  }
  match req.gateway.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
    Some(gateway) if !settings.as_gateway => args.extend(["--gateway".into(), gateway.into()]),
    None if !settings.as_gateway && settings.auto_gateway => args.push("--auto-gateway".into()),
    _ => {}
  }
  if let Some(user) = req.username.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
    args.extend(["--user".into(), user.into()]);
  }
  if req.password.as_deref().is_some_and(|p| !p.is_empty()) {
    args.push("--passwd-on-stdin".into());
  }
  if settings.use_default_browser {
    args.push("--browser".into());
  }
  if settings.hip {
    args.push("--hip".into());
  }
  if settings.no_dtls {
    args.push("--no-dtls".into());
  }
  if settings.disable_ipv6 {
    args.push("--disable-ipv6".into());
  }
  args.extend(["--os".into(), settings.os.as_arg().into()]);
  let client_version = settings.client_version.trim();
  if !client_version.is_empty() {
    args.extend(["--client-version".into(), client_version.into()]);
  }
  if settings.mtu > 0 {
    args.extend(["--mtu".into(), settings.mtu.to_string()]);
  }
  args.extend(["--reconnect-timeout".into(), settings.reconnect_timeout.to_string()]);
  args.extend(settings.extra_args.split_whitespace().map(String::from));

  // The portal is positional and must come last so extra args cannot swallow it.
  args.push(req.portal.trim().to_owned());
  args
}

fn parse_line(line: &str) -> (LogLevel, String) {
  match serde_json::from_str::<JsonRecord>(line) {
    Ok(rec) => {
      let level = match rec.level.as_str() {
        "ERROR" => LogLevel::Error,
        "WARN" => LogLevel::Warn,
        "INFO" => LogLevel::Info,
        _ => LogLevel::Debug,
      };
      (level, rec.message)
    }
    // pkexec, gpauth and openconnect may print plain text.
    Err(_) => (LogLevel::Info, line.to_owned()),
  }
}

fn pump<R: Read + Send + 'static>(
  reader: R,
  tx: async_channel::Sender<VpnEvent>,
) -> thread::JoinHandle<Option<String>> {
  thread::spawn(move || {
    let mut last_error = None;
    for line in BufReader::new(reader).lines() {
      let Ok(line) = line else { break };
      let line = line.trim_end();
      if line.is_empty() {
        continue;
      }
      let (level, message) = parse_line(line);
      if level == LogLevel::Error {
        last_error = Some(message.clone());
      }
      if message.contains(CONNECTED_MARKER) {
        let _ = tx.send_blocking(VpnEvent::Connected);
      }
      if tx.send_blocking(VpnEvent::Log { level, message }).is_err() {
        break;
      }
    }
    last_error
  })
}

fn describe_exit(code: Option<i32>) -> Option<String> {
  match code {
    Some(0) => None,
    Some(126) => Some("Authorization was cancelled".into()),
    Some(127) => Some("Not authorized to run gpclient (polkit)".into()),
    Some(code) => Some(format!("gpclient exited with code {code}")),
    None => Some("gpclient was terminated by a signal".into()),
  }
}

/// Start `pkexec gpclient … connect` and stream events to `tx` until it exits.
pub fn spawn_connect(
  gpclient: &Path,
  settings: &Settings,
  req: ConnectRequest,
  tx: async_channel::Sender<VpnEvent>,
) -> anyhow::Result<()> {
  let args = connect_args(settings, &req);
  // The password travels on stdin only, so the argument list is safe to log.
  log::info!("Running pkexec {} {}", gpclient.display(), args.join(" "));

  let mut child: Child = Command::new("pkexec")
    .arg(gpclient)
    .args(&args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .context("Failed to start pkexec")?;

  if let Some(mut stdin) = child.stdin.take() {
    if let Some(password) = req.password.filter(|p| !p.is_empty()) {
      // Ignore EPIPE: gpclient may exit before reading (e.g. auth cancelled).
      let _ = stdin
        .write_all(password.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"));
    }
    drop(stdin);
  }

  let out = pump(child.stdout.take().expect("piped stdout"), tx.clone());
  let err = pump(child.stderr.take().expect("piped stderr"), tx.clone());

  thread::spawn(move || {
    let status = child.wait();
    let last_err = err.join().ok().flatten();
    let last_out = out.join().ok().flatten();
    let event = match status {
      Ok(status) if status.success() => VpnEvent::Exited {
        success: true,
        error: None,
      },
      Ok(status) => VpnEvent::Exited {
        success: false,
        error: last_err.or(last_out).or_else(|| describe_exit(status.code())),
      },
      Err(e) => VpnEvent::Exited {
        success: false,
        error: Some(e.to_string()),
      },
    };
    let _ = tx.send_blocking(event);
  });
  Ok(())
}

/// Blocking: ask the running gpclient to disconnect (`gpclient disconnect`).
pub fn disconnect(gpclient: &Path) -> anyhow::Result<()> {
  let output = Command::new("pkexec")
    .arg(gpclient)
    .arg("disconnect")
    .stdin(Stdio::null())
    .output()
    .context("Failed to start pkexec")?;
  if !output.status.success() {
    let reason = describe_exit(output.status.code()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&output.stderr);
    match stderr.trim() {
      "" => bail!("{reason}"),
      detail => bail!("{reason}: {detail}"),
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn req() -> ConnectRequest {
    ConnectRequest {
      portal: " vpn.example.com ".into(),
      gateway: None,
      username: Some("alice".into()),
      password: Some("secret".into()),
    }
  }

  #[test]
  fn default_args_use_json_logs_auto_gateway_and_stdin_password() {
    let args = connect_args(&Settings::default(), &req());
    assert_eq!(&args[..3], ["--log-format", "json", "connect"]);
    assert!(args.contains(&"--auto-gateway".to_string()));
    assert!(args.contains(&"--passwd-on-stdin".to_string()));
    assert!(
      !args.iter().any(|a| a == "secret"),
      "password must never be an argument"
    );
    assert_eq!(args.last().unwrap(), "vpn.example.com");
  }

  #[test]
  fn explicit_gateway_overrides_auto_gateway() {
    let mut r = req();
    r.gateway = Some("gw1".into());
    let args = connect_args(&Settings::default(), &r);
    assert!(!args.contains(&"--auto-gateway".to_string()));
    let i = args.iter().position(|a| a == "--gateway").unwrap();
    assert_eq!(args[i + 1], "gw1");
  }

  #[test]
  fn global_flags_precede_subcommand() {
    let settings = Settings {
      ignore_tls_errors: true,
      ..Default::default()
    };
    let args = connect_args(&settings, &req());
    assert_eq!(args[0], "--ignore-tls-errors");
    assert!(args.iter().position(|a| a == "connect").unwrap() > 0);
  }

  #[test]
  fn as_gateway_skips_gateway_selection() {
    let settings = Settings {
      as_gateway: true,
      ..Default::default()
    };
    let mut r = req();
    r.gateway = Some("gw1".into());
    let args = connect_args(&settings, &r);
    assert!(args.contains(&"--as-gateway".to_string()));
    assert!(!args.contains(&"--gateway".to_string()));
    assert!(!args.contains(&"--auto-gateway".to_string()));
  }

  #[test]
  fn parses_json_and_plain_lines() {
    let (level, msg) = parse_line(r#"{"timestamp":"t","level":"ERROR","target":"x","message":"boom"}"#);
    assert_eq!(level, LogLevel::Error);
    assert_eq!(msg, "boom");
    let (level, msg) = parse_line("Error executing command as another user");
    assert_eq!(level, LogLevel::Info);
    assert_eq!(msg, "Error executing command as another user");
  }

  #[test]
  fn pkexec_exit_codes_are_explained() {
    assert!(describe_exit(Some(0)).is_none());
    assert!(describe_exit(Some(126)).unwrap().contains("cancelled"));
  }
}
