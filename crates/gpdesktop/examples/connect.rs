//! Headless smoke test of the desktop core against a real gpservice:
//!
//! ```text
//! set GP_PASSWORD=...            (optional, for non-SSO portals)
//! cargo run -p gpdesktop --example connect -- <portal> [username] [gateway]
//! ```
//!
//! Signs in (SAML opens gpauth), asks the service for the tunnel and prints
//! every status change. Ctrl+C disconnects and exits.

use std::io::{BufRead, Write};

use gpdesktop::{
  config::Settings,
  controller::{self, Command, FormInput, UiEvent},
};

fn main() -> anyhow::Result<()> {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
  let mut args = std::env::args().skip(1);
  let portal = args
    .next()
    .ok_or_else(|| anyhow::anyhow!("usage: connect <portal> [username] [gateway]"))?;
  let username = args.next().unwrap_or_default();
  let gateway = args.next().unwrap_or_default();

  let rt = tokio::runtime::Runtime::new()?;
  let (exit_tx, exit_rx) = std::sync::mpsc::channel();
  let handle = controller::spawn(rt.handle(), Settings::load(), move |event| match event {
    UiEvent::Status(view) => println!("[status] {} — {}", view.title, view.detail),
    UiEvent::Log(level, message) => println!("[{}] {message}", level.tag()),
    UiEvent::Toast(message) => println!("[toast] {message}"),
    UiEvent::AskOtp { message, reply } => {
      print!("{message}: ");
      let _ = std::io::stdout().flush();
      let mut code = String::new();
      let _ = std::io::stdin().lock().read_line(&mut code);
      let _ = reply.send(Some(code.trim().to_owned()));
    }
    UiEvent::ConfirmStopOthers { processes, reply, .. } => {
      for p in &processes {
        println!("[other] {}", p.describe());
      }
      let _ = reply.send(false);
    }
    UiEvent::ServiceNotRunning(reason) => println!("[service] {reason}"),
    UiEvent::Settings(_) => {}
    UiEvent::Exit => {
      let _ = exit_tx.send(());
    }
  });

  let quit = handle.clone();
  rt.spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    println!("Disconnecting…");
    quit.send(Command::Quit { disconnect: true });
  });

  handle.send(Command::Connect(FormInput {
    portal,
    gateway,
    username,
    password: std::env::var("GP_PASSWORD").unwrap_or_default(),
    remember_password: false,
  }));
  let _ = exit_rx.recv();
  Ok(())
}
