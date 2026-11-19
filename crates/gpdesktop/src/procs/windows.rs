use std::{ffi::OsString, os::windows::ffi::OsStringExt};

use anyhow::bail;
use windows_sys::Win32::{
  Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
  System::{
    Diagnostics::ToolHelp::{
      CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    },
    Threading::{
      GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
      QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    },
  },
  UI::{
    Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
    WindowsAndMessaging::SW_HIDE,
  },
};

use super::VpnProcess;

/// Executables that can hold a tunnel adapter or an auth window: other copies
/// of this project's CLI/auth helper and a standalone OpenConnect (as gpgui-gtk).
/// Our own `gpservice` is never listed, and neither is the Palo Alto
/// GlobalProtect agent (PanGPS/PanGPA): it is often a managed corporate service.
const VPN_PROCESS_NAMES: &[&str] = &["gpclient.exe", "gpauth.exe", "openconnect.exe"];

fn is_vpn_name(name: &str) -> bool {
  VPN_PROCESS_NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

fn image_path(pid: u32) -> String {
  unsafe {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process.is_null() {
      return String::new();
    }
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
    CloseHandle(process);
    if ok == 0 {
      return String::new();
    }
    OsString::from_wide(&buffer[..len as usize])
      .to_string_lossy()
      .into_owned()
  }
}

/// Running VPN processes (see [`VPN_PROCESS_NAMES`]), except `exclude` and children we launched.
pub fn find_vpn_processes(exclude: &[u32]) -> Vec<VpnProcess> {
  let me = std::process::id();
  let mut found = Vec::new();
  unsafe {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snapshot == INVALID_HANDLE_VALUE {
      return found;
    }
    let mut entry: PROCESSENTRY32W = std::mem::zeroed();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = Process32FirstW(snapshot, &mut entry);
    while ok != 0 {
      let len = entry
        .szExeFile
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(entry.szExeFile.len());
      let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
      let pid = entry.th32ProcessID;
      if is_vpn_name(&name) && pid != me && entry.th32ParentProcessID != me && !exclude.contains(&pid) {
        found.push(VpnProcess {
          pid,
          name,
          detail: image_path(pid),
        });
      }
      ok = Process32NextW(snapshot, &mut entry);
    }
    CloseHandle(snapshot);
  }
  found.sort_by_key(|p| p.pid);
  found
}

/// Blocking: terminate the given processes. Processes we may not open (other
/// users, services) are stopped with an elevated `taskkill` (one UAC prompt).
pub fn kill_processes(pids: &[u32]) -> anyhow::Result<()> {
  let mut denied = Vec::new();
  for &pid in pids {
    unsafe {
      let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
      if process.is_null() {
        if GetLastError() == ERROR_ACCESS_DENIED {
          denied.push(pid);
        }
        continue; // Already gone.
      }
      let ok = TerminateProcess(process, 1);
      if ok == 0 && GetLastError() == ERROR_ACCESS_DENIED {
        denied.push(pid);
      } else {
        WaitForSingleObject(process, 5000);
      }
      CloseHandle(process);
    }
  }
  if !denied.is_empty() {
    elevated_taskkill(&denied)?;
  }
  Ok(())
}

fn elevated_taskkill(pids: &[u32]) -> anyhow::Result<()> {
  let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
  let args: String = std::iter::once("/F".to_owned())
    .chain(pids.iter().map(|pid| format!("/PID {pid}")))
    .collect::<Vec<_>>()
    .join(" ");
  let (verb, file, params) = (wide("runas"), wide("taskkill.exe"), wide(&args));
  unsafe {
    let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if ShellExecuteExW(&mut info) == 0 {
      bail!("Stopping the processes as administrator was cancelled");
    }
    if info.hProcess.is_null() {
      return Ok(());
    }
    let waited = WaitForSingleObject(info.hProcess, 15_000);
    let mut code = 0u32;
    GetExitCodeProcess(info.hProcess, &mut code);
    CloseHandle(info.hProcess);
    if waited != WAIT_OBJECT_0 {
      bail!("taskkill did not finish");
    }
    // 128 = process not found (already exited), which is fine.
    if code != 0 && code != 128 {
      bail!("taskkill failed with code {code}");
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn names_match_case_insensitively() {
    assert!(is_vpn_name("GPAUTH.EXE"));
    assert!(is_vpn_name("OpenConnect.exe"));
    assert!(!is_vpn_name("PanGPS.exe"));
    assert!(!is_vpn_name("gpservice.exe"));
    assert!(!is_vpn_name("explorer.exe"));
  }

  #[test]
  fn never_lists_itself() {
    let found = find_vpn_processes(&[]);
    assert!(found.iter().all(|p| p.pid != std::process::id()));
    assert!(found.iter().all(|p| is_vpn_name(&p.name)));
  }
}
