//! GlobalProtect VPN — open-source native Windows GUI (Win32 + Direct2D).
//!
//! Feature parity with gpgui-gtk: the connection logic lives in the shared
//! `gpdesktop` crate; this binary only renders it. On other platforms it is a
//! stub so `cargo build --workspace` keeps working.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
  win::main()
}

#[cfg(not(windows))]
fn main() {
  eprintln!("gpgui-win only runs on Windows; use gpgui-gtk on Linux/BSD.");
  std::process::exit(1);
}
