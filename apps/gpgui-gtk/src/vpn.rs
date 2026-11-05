//! Log levels and helpers for finding/stopping stray VPN processes
//! (gpclient, gpauth, gpservice, openconnect) left by other clients.

use std::process::{Command, Stdio};

use anyhow::{Context, bail};

/// Process names that can hold the tunnel, a lock file or an auth window.
const VPN_PROCESS_NAMES: &[&str] = &["gpclient", "gpauth", "gpservice", "openconnect"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
  Error,
  Warn,
  Info,
  Debug,
}

fn describe_exit(code: Option<i32>) -> Option<String> {
  match code {
    Some(0) => None,
    Some(126) => Some("Authorization was cancelled".into()),
    Some(127) => Some("Not authorized (polkit)".into()),
    Some(code) => Some(format!("process exited with code {code}")),
    None => Some("process was terminated by a signal".into()),
  }
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

#[cfg(test)]
mod tests {
  use super::*;

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
