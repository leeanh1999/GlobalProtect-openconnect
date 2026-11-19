//! Windows bootstrap: the `GPConnect` service runs as LocalSystem under the
//! Service Control Manager. Any interactive user may ask its admission pipe for
//! a session credential; the pipe must stay open for the session's lifetime.
//! Closing this app does not stop the service or the tunnel.

use std::time::Duration;

use anyhow::{Context, bail};
use gpapi::service::transport::WINDOWS_ADMISSION_REQUEST;
use tokio::io::AsyncWriteExt;

use super::{Bootstrap, Lease, PendingBoot, read_credential};

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
  NotInstalled,
  Stopped,
  Running,
}

/// Whether the `GPConnect` service is installed and running.
pub fn status() -> anyhow::Result<ServiceStatus> {
  match gpapi::windows::service_process_id(common::paths::SERVICE_NAME) {
    Ok(0) => Ok(ServiceStatus::Stopped),
    Ok(_) => Ok(ServiceStatus::Running),
    Err(err) if err.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) => Ok(ServiceStatus::NotInstalled),
    Err(err) => Err(err).context("Cannot query the VPN service"),
  }
}

/// Asks Windows (UAC prompt) to start the service. Returns once it runs.
pub async fn request_start() -> anyhow::Result<()> {
  use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_HIDE};
  let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
  let (verb, file, args) = (
    wide("runas"),
    wide("sc.exe"),
    wide(&format!("start {}", common::paths::SERVICE_NAME)),
  );
  let code = unsafe {
    ShellExecuteW(
      std::ptr::null_mut(),
      verb.as_ptr(),
      file.as_ptr(),
      args.as_ptr(),
      std::ptr::null(),
      SW_HIDE,
    )
  } as isize;
  if code <= 32 {
    bail!("Starting the VPN service was cancelled or failed (code {code})");
  }
  for _ in 0..40 {
    if status()? == ServiceStatus::Running {
      return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
  }
  bail!("The VPN service did not start")
}

pub(crate) fn bootstrap() -> anyhow::Result<(Option<u32>, PendingBoot)> {
  match status()? {
    ServiceStatus::Running => {}
    ServiceStatus::Stopped => bail!(ServiceNotRunning),
    ServiceStatus::NotInstalled => bail!("The VPN service (GPConnect) is not installed. Reinstall GlobalProtect VPN."),
  }
  // Must run inside the tokio runtime: the pipe registers with its reactor.
  let mut pipe = gpapi::windows::connect_service_admission().context("Cannot open the VPN service admission pipe")?;
  let boot = async move {
    pipe.write_all(&WINDOWS_ADMISSION_REQUEST).await?;
    let credential = tokio::time::timeout(Duration::from_secs(10), read_credential(&mut pipe))
      .await
      .context("The VPN service did not issue a session credential in time")??
      .context("The VPN service closed the admission pipe")?;
    Ok(Bootstrap {
      credential,
      lease: Lease::Pipe(pipe),
    })
  };
  Ok((None, Box::pin(boot)))
}

/// The service is installed but stopped; the UI may offer [`request_start`].
#[derive(Debug)]
pub struct ServiceNotRunning;

impl std::fmt::Display for ServiceNotRunning {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("The VPN service (GPConnect) is not running")
  }
}

impl std::error::Error for ServiceNotRunning {}
