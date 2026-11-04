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
/// gpclient hint printed when the server needs legacy OpenSSL settings (crates/gpapi/src/clap/mod.rs).
const FIX_OPENSSL_MARKER: &str = "--fix-openssl";
/// gpclient refuses to start while its lock file points to a live gpclient (apps/gpclient/src/cli.rs).
const ANOTHER_INSTANCE_MARKER: &str = "Another instance of the client is already running";
/// Process names that can hold the tunnel, the gpclient lock or an auth window.
const VPN_PROCESS_NAMES: &[&str] = &["gpclient", "gpauth", "gpservice", "openconnect"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
  Error,
  Warn,
  Info,
  Debug,
}

#[derive(Debug, Clone)]
pub enum VpnEvent {
  Log {
    level: LogLevel,
    message: String,
  },
  Connected,
  /// The server needs `--fix-openssl` (legacy TLS renegotiation / ciphers).
  NeedsFixOpenssl,
  /// Another gpclient holds the lock file.
  AnotherInstance,
  Exited {
    success: bool,
    error: Option<String>,
  },
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
  // Global flags must precede the subcommand.
  if settings.fix_openssl {
    args.push("--fix-openssl".into());
  }
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
      if message.contains(FIX_OPENSSL_MARKER) {
        let _ = tx.send_blocking(VpnEvent::NeedsFixOpenssl);
      }
      if message.contains(ANOTHER_INSTANCE_MARKER) {
        let _ = tx.send_blocking(VpnEvent::AnotherInstance);
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
/// Returns the child pid (pkexec execs gpclient in place, so it is gpclient's pid).
pub fn spawn_connect(
  gpclient: &Path,
  settings: &Settings,
  req: ConnectRequest,
  tx: async_channel::Sender<VpnEvent>,
) -> anyhow::Result<u32> {
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

  let pid = child.id();
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
  Ok(pid)
}

#[derive(Debug, Clone)]
pub struct VpnProcess {
  pub pid: u32,
  pub name: String,
  pub cmdline: String,
}

/// Running gpclient/gpauth/gpservice/openconnect processes, except `exclude`.
pub fn find_vpn_processes(exclude: &[u32]) -> Vec<VpnProcess> {
  let Ok(entries) = std::fs::read_dir("/proc") else {
    return Vec::new();
  };
  let mut found: Vec<VpnProcess> = entries
    .flatten()
    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    .filter(|pid| *pid != std::process::id() && !exclude.contains(pid))
    .filter_map(|pid| {
      let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
      let name = name.trim();
      if !VPN_PROCESS_NAMES.contains(&name) {
        return None;
      }
      let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " ").trim().to_owned())
        .unwrap_or_default();
      Some(VpnProcess {
        pid,
        name: name.to_owned(),
        cmdline,
      })
    })
    .collect();
  found.sort_by_key(|p| p.pid);
  found
}

/// Root helper run through pkexec: SIGINT (graceful disconnect), wait up to 5 s,
/// then SIGKILL survivors and drop a stale gpclient lock. One auth prompt total.
const KILL_SCRIPT: &str = r#"
for p in "$@"; do kill -INT "$p" 2>/dev/null; done
i=0
while [ "$i" -lt 20 ]; do
  alive=0
  for p in "$@"; do kill -0 "$p" 2>/dev/null && alive=1; done
  [ "$alive" = 0 ] && break
  sleep 0.25
  i=$((i + 1))
done
for p in "$@"; do kill -KILL "$p" 2>/dev/null; done
rm -f /var/run/gpclient.lock /run/gpclient.lock
exit 0
"#;

/// Blocking: stop the given processes as root.
pub fn kill_processes(pids: &[u32]) -> anyhow::Result<()> {
  if pids.is_empty() {
    return Ok(());
  }
  let output = Command::new("pkexec")
    .args(["/bin/sh", "-c", KILL_SCRIPT, "gpgui-gtk-kill"])
    .args(pids.iter().map(u32::to_string))
    .stdin(Stdio::null())
    .output()
    .context("Failed to start pkexec")?;
  if !output.status.success() {
    bail!(
      "{}",
      describe_exit(output.status.code()).unwrap_or_else(|| "Failed to stop processes".into())
    );
  }
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
  fn fix_openssl_is_a_global_flag() {
    let settings = Settings {
      fix_openssl: true,
      ..Default::default()
    };
    let args = connect_args(&settings, &req());
    let fix = args.iter().position(|a| a == "--fix-openssl").unwrap();
    let connect = args.iter().position(|a| a == "connect").unwrap();
    assert!(fix < connect);
    assert!(!connect_args(&Settings::default(), &req()).contains(&"--fix-openssl".to_string()));
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
  fn finds_processes_by_name_and_honours_exclusions() {
    // Spawn something that is not a VPN process: must never be reported.
    let mut child = Command::new("sleep").arg("5").spawn().unwrap();
    let found = find_vpn_processes(&[]);
    assert!(found.iter().all(|p| VPN_PROCESS_NAMES.contains(&p.name.as_str())));
    assert!(found.iter().all(|p| p.pid != child.id() && p.pid != std::process::id()));
    child.kill().ok();
    child.wait().ok();
  }

  #[test]
  fn pkexec_exit_codes_are_explained() {
    assert!(describe_exit(Some(0)).is_none());
    assert!(describe_exit(Some(126)).unwrap().contains("cancelled"));
  }
}
