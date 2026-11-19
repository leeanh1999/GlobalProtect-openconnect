//! Linux/BSD bootstrap (as in gpgui-gtk): a private gpservice is started with
//! `pkexec gpservice --desktop-credentials` (the upstream polkit action
//! `com.yuezk.gpgui.service` allows active local users without a password).
//! Its stdout carries the credential frame; closing its stdin revokes the
//! session, stops the VPN and exits the service.

use std::{path::PathBuf, process::Stdio};

use anyhow::{Context, ensure};
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::Command,
  sync::mpsc,
};

use super::{Bootstrap, Lease, PendingBoot, ServiceEvent, read_credential};
use crate::LogLevel;

pub fn resolve_gpservice(configured: &str) -> anyhow::Result<PathBuf> {
  let configured = configured.trim();
  if !configured.is_empty() {
    let path = PathBuf::from(configured);
    ensure!(path.is_absolute(), "gpservice path must be absolute: {configured}");
    ensure!(path.is_file(), "gpservice not found at {configured}");
    return Ok(path);
  }
  ["/usr/bin/gpservice", "/usr/local/bin/gpservice"]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
    .context("gpservice was not found. Install GlobalProtect-openconnect or set its path in Preferences")
}

pub(crate) fn bootstrap(
  gpservice_path: &str,
  events: &mpsc::UnboundedSender<ServiceEvent>,
) -> anyhow::Result<(Option<u32>, PendingBoot)> {
  let gpservice = resolve_gpservice(gpservice_path)?;
  let _ = events.send(ServiceEvent::Log(
    LogLevel::Info,
    format!("Starting {}", gpservice.display()),
  ));
  let mut child = Command::new("pkexec")
    .arg("--user")
    .arg("root")
    .arg(&gpservice)
    .arg("--desktop-credentials")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(false)
    .spawn()
    .context("Failed to start pkexec")?;
  let pid = child.id();

  if let Some(stderr) = child.stderr.take() {
    let events = events.clone();
    tokio::spawn(async move {
      let mut lines = BufReader::new(stderr).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end();
        if !line.is_empty() {
          let level = if line.contains("ERROR") {
            LogLevel::Error
          } else if line.contains("WARN") {
            LogLevel::Warn
          } else {
            LogLevel::Debug
          };
          let _ = events.send(ServiceEvent::Log(level, format!("[gpservice] {line}")));
        }
      }
    });
  }

  let boot = async move {
    let stdin = child.stdin.take().context("Service input pipe is unavailable")?;
    let mut output = child.stdout.take().context("Service credential pipe is unavailable")?;
    let credential = tokio::select! {
      result = read_credential(&mut output) => result?.context("gpservice exited before issuing a session credential")?,
      status = child.wait() => anyhow::bail!("gpservice exited during startup ({})", status?),
    };
    Ok(Bootstrap {
      credential,
      lease: Lease::Child {
        child,
        _stdin: stdin,
        _stdout: output,
      },
    })
  };
  Ok((pid, Box::pin(boot)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn configured_gpservice_path_must_be_absolute_and_exist() {
    assert!(resolve_gpservice("gpservice").is_err());
    assert!(resolve_gpservice("/nonexistent/gpservice").is_err());
    assert_eq!(resolve_gpservice("/bin/sh").unwrap(), PathBuf::from("/bin/sh"));
  }
}
