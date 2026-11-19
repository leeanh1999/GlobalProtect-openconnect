//! Password storage in the desktop Secret Service (GNOME Keyring, KWallet, …)
//! or, inside a Flatpak sandbox, the portal-backed file keyring.

use std::sync::OnceLock;

use anyhow::Context;

const SCHEMA_KEY: &str = "xdg:schema";
const SCHEMA: &str = "io.github.gpgui_gtk.Password";

fn runtime() -> &'static tokio::runtime::Runtime {
  static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
  RT.get_or_init(|| {
    tokio::runtime::Builder::new_multi_thread()
      .worker_threads(1)
      .enable_all()
      .build()
      .expect("tokio runtime")
  })
}

fn attributes<'a>(portal: &'a str, username: &'a str) -> Vec<(&'a str, &'a str)> {
  vec![(SCHEMA_KEY, SCHEMA), ("portal", portal), ("username", username)]
}

/// Blocking: call from a worker thread (`gio::spawn_blocking`), never the GTK main thread.
pub fn load(portal: &str, username: &str) -> anyhow::Result<Option<String>> {
  runtime().block_on(async {
    let keyring = oo7::Keyring::new().await.context("open keyring")?;
    keyring.unlock().await.ok();
    let items = keyring.search_items(&attributes(portal, username)).await?;
    let Some(item) = items.first() else {
      return Ok(None);
    };
    let secret = item.secret().await?;
    Ok(Some(String::from_utf8(secret.as_bytes().to_vec())?))
  })
}

/// Blocking: call from a worker thread.
pub fn store(portal: &str, username: &str, password: &str) -> anyhow::Result<()> {
  runtime().block_on(async {
    let keyring = oo7::Keyring::new().await.context("open keyring")?;
    keyring
      .create_item(
        &format!("GlobalProtect password for {username}@{portal}"),
        &attributes(portal, username),
        password.to_owned(),
        true,
      )
      .await?;
    Ok(())
  })
}

/// Blocking: call from a worker thread.
pub fn delete(portal: &str, username: &str) -> anyhow::Result<()> {
  runtime().block_on(async {
    let keyring = oo7::Keyring::new().await.context("open keyring")?;
    keyring.delete(&attributes(portal, username)).await?;
    Ok(())
  })
}
