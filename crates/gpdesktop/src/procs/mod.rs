//! Finding and stopping stray VPN processes left by other clients, which can
//! hold the tunnel, a lock file or an auth window and block a new connection.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{find_vpn_processes, kill_processes};
#[cfg(windows)]
pub use windows::{find_vpn_processes, kill_processes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpnProcess {
  pub pid: u32,
  pub name: String,
  /// Command line (Linux) or executable path (Windows).
  pub detail: String,
}

impl VpnProcess {
  /// One line for a confirmation dialog.
  pub fn describe(&self) -> String {
    let detail: String = self.detail.chars().take(80).collect();
    if detail.is_empty() {
      format!("• {} (pid {})", self.name, self.pid)
    } else {
      format!("• {} (pid {})  {detail}", self.name, self.pid)
    }
  }
}
