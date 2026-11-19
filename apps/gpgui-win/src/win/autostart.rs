//! "Start with Windows": a per-user `Run` entry that opens the app hidden in
//! the notification area (`--minimized`).

use anyhow::Context;
use windows::{
  Win32::{
    Foundation::ERROR_FILE_NOT_FOUND,
    System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW},
  },
  core::{HSTRING, w},
};

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: windows::core::PCWSTR = w!("GlobalProtect VPN");

/// The command registered for this executable.
fn command() -> anyhow::Result<String> {
  let exe = std::env::current_exe().context("Cannot locate the app executable")?;
  Ok(format!("\"{}\" --minimized", exe.display()))
}

pub fn is_enabled() -> bool {
  unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, None) }.is_ok()
}

pub fn set(enabled: bool) -> anyhow::Result<()> {
  if enabled {
    let command = HSTRING::from(command()?);
    // REG_SZ data includes the terminating NUL.
    let bytes = (command.len() + 1) * 2;
    unsafe {
      RegSetKeyValueW(
        HKEY_CURRENT_USER,
        RUN_KEY,
        VALUE,
        REG_SZ.0,
        Some(command.as_ptr() as *const _),
        bytes as u32,
      )
    }
    .ok()
    .context("Cannot enable Start with Windows")?;
  } else {
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
    if status.is_err() && status != ERROR_FILE_NOT_FOUND {
      status.ok().context("Cannot disable Start with Windows")?;
    }
  }
  Ok(())
}

/// Keeps an enabled entry pointing at the running executable (e.g. after the
/// app was reinstalled to another folder).
pub fn refresh() {
  if !is_enabled() {
    return;
  }
  let current = command().ok();
  let mut buf = vec![0u16; 1024];
  let mut size = (buf.len() * 2) as u32;
  let ok = unsafe {
    RegGetValueW(
      HKEY_CURRENT_USER,
      RUN_KEY,
      VALUE,
      RRF_RT_REG_SZ,
      None,
      Some(buf.as_mut_ptr() as *mut _),
      Some(&mut size),
    )
  }
  .is_ok();
  let stored = ok.then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]));
  if current.is_some()
    && stored != current
    && let Err(err) = set(true)
  {
    log::warn!("{err:#}");
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn enable_and_disable_round_trip() {
    if is_enabled() {
      eprintln!("skipped: Start with Windows is enabled for this user");
      return;
    }
    set(true).unwrap();
    assert!(is_enabled());
    set(false).unwrap();
    assert!(!is_enabled());
    set(false).unwrap(); // Disabling twice is fine.
  }
}
