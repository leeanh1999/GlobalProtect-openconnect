mod auth;
mod config;
mod icons;
mod preferences;
mod secrets;
mod service;
mod state;
mod tray;
mod vpn;
mod window;

use adw::prelude::*;

fn main() -> gtk::glib::ExitCode {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  if std::env::args().any(|a| a == "--version") {
    println!("gpgui-gtk {}", env!("CARGO_PKG_VERSION"));
    return gtk::glib::ExitCode::SUCCESS;
  }

  // OpenSSL reads OPENSSL_CONF once, before the first TLS connection (portal/gateway auth).
  if config::Settings::load().fix_openssl {
    match gpapi::utils::openssl::fix_openssl_env() {
      // Keep the temporary config file for the life of the process.
      Ok(file) => std::mem::forget(file),
      Err(err) => log::warn!("Cannot enable legacy OpenSSL mode: {err:#}"),
    }
  }

  let app = adw::Application::builder().application_id(config::APP_ID).build();
  window::setup(&app);
  // GTK parses argv itself; we only accept GApplication's own options.
  app.run_with_args(&std::env::args().take(1).collect::<Vec<_>>())
}
