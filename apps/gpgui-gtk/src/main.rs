//! GlobalProtect VPN — open-source GTK4/libadwaita GUI (Linux/BSD).
//! On Windows this binary is a stub; use gpgui-win there.

#[cfg(not(windows))]
mod auth;
#[cfg(not(windows))]
mod config;
#[cfg(not(windows))]
mod icons;
#[cfg(not(windows))]
mod preferences;
#[cfg(not(windows))]
mod secrets;
#[cfg(not(windows))]
mod service;
#[cfg(not(windows))]
mod state;
#[cfg(not(windows))]
mod tray;
#[cfg(not(windows))]
mod vpn;
#[cfg(not(windows))]
mod window;

#[cfg(not(windows))]
fn main() -> gtk::glib::ExitCode {
  use adw::prelude::*;

  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  if std::env::args().any(|a| a == "--version") {
    println!("gpgui-gtk {}", env!("CARGO_PKG_VERSION"));
    return gtk::glib::ExitCode::SUCCESS;
  }

  // Legacy TLS renegotiation is handled by gpapi itself since 3.x (`--fix-openssl` is a no-op).
  let app = adw::Application::builder().application_id(config::APP_ID).build();
  window::setup(&app);
  // GTK parses argv itself; we only accept GApplication's own options.
  app.run_with_args(&std::env::args().take(1).collect::<Vec<_>>())
}

#[cfg(windows)]
fn main() {
  eprintln!("gpgui-gtk runs on Linux/BSD; use gpgui-win on Windows.");
  std::process::exit(1);
}
