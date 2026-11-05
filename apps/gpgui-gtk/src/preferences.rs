use adw::prelude::*;

use crate::config::{ClientOs, Settings};

fn switch(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
  adw::SwitchRow::builder()
    .title(title)
    .subtitle(subtitle)
    .active(active)
    .build()
}

fn spin(title: &str, subtitle: &str, min: f64, max: f64, value: u32) -> adw::SpinRow {
  let row = adw::SpinRow::with_range(min, max, 1.0);
  row.set_title(title);
  row.set_subtitle(subtitle);
  row.set_value(value as f64);
  row
}

fn entry(title: &str, text: &str) -> adw::EntryRow {
  adw::EntryRow::builder().title(title).text(text).build()
}

/// Shows the preferences dialog; `on_close` receives the edited settings.
pub fn present(parent: &impl IsA<gtk::Widget>, s: Settings, on_close: impl Fn(Settings) + 'static) {
  // Connection
  let as_gateway = switch(
    "Connect as gateway",
    "Treat the address as a gateway, not a portal",
    s.as_gateway,
  );
  let auto_gateway = switch(
    "Automatic gateway",
    "Try gateways by priority when none is given",
    s.auto_gateway,
  );
  let browser = switch(
    "Use default browser for SSO",
    "Authenticate SAML in the system browser instead of the embedded window",
    s.use_default_browser,
  );
  let hip = switch(
    "Send HIP report",
    "Host Information Profile, if the gateway requires it",
    s.hip,
  );
  let ignore_tls = switch(
    "Ignore TLS errors",
    "Insecure: accept invalid server certificates",
    s.ignore_tls_errors,
  );
  let connection = adw::PreferencesGroup::builder().title("Connection").build();
  let fix_openssl = switch(
    "Legacy OpenSSL compatibility",
    "For servers that need legacy TLS renegotiation (gpclient --fix-openssl)",
    s.fix_openssl,
  );
  for row in [&as_gateway, &auto_gateway, &browser, &hip, &ignore_tls, &fix_openssl] {
    connection.add(row);
  }

  // Tunnel
  let no_dtls = switch("Disable DTLS and ESP", "Use TLS only", s.no_dtls);
  let ipv4_only = switch(
    "IPv4 only",
    "Do not route IPv6 traffic through the tunnel",
    s.disable_ipv6,
  );
  let mtu = spin("MTU", "0 = server default (legacy servers only)", 0.0, 9000.0, s.mtu);
  let reconnect = spin(
    "Tunnel retry timeout",
    "Seconds gpclient keeps retrying a dropped tunnel before giving up",
    0.0,
    86400.0,
    s.reconnect_timeout,
  );
  let tunnel = adw::PreferencesGroup::builder().title("Tunnel").build();
  for row in [
    no_dtls.upcast_ref::<gtk::Widget>(),
    ipv4_only.upcast_ref(),
    mtu.upcast_ref(),
    reconnect.upcast_ref(),
  ] {
    tunnel.add(row);
  }

  // Client identity
  let os_model = gtk::StringList::new(&ClientOs::ALL.map(|os| os.as_arg()));
  let os = adw::ComboRow::builder()
    .title("Reported OS")
    .model(&os_model)
    .selected(s.os.index())
    .build();
  let client_version = entry("Client version (optional, e.g. 6.2.4-49)", &s.client_version);
  let identity = adw::PreferencesGroup::builder().title("Client identity").build();
  identity.add(&os);
  identity.add(&client_version);

  // Advanced
  let gpclient_path = entry("gpclient path (empty = auto-detect)", &s.gpclient_path);
  let extra_args = entry("Extra gpclient connect arguments", &s.extra_args);
  let advanced = adw::PreferencesGroup::builder().title("Advanced").build();
  advanced.add(&gpclient_path);
  advanced.add(&extra_args);

  // Auto reconnect (handled by the GUI after gpclient exits)
  let auto_reconnect = switch(
    "Reconnect automatically",
    "Start a new connection when an established one drops",
    s.auto_reconnect,
  );
  let reconnect_delay = spin(
    "First retry after",
    "Seconds; doubles after each failed attempt, up to 5 minutes",
    1.0,
    300.0,
    s.reconnect_delay,
  );
  let reconnect_max = spin(
    "Maximum attempts",
    "Give up after this many failures in a row (0 = keep trying)",
    0.0,
    1000.0,
    s.reconnect_max_attempts,
  );
  auto_reconnect
    .bind_property("active", &reconnect_delay, "sensitive")
    .sync_create()
    .build();
  auto_reconnect
    .bind_property("active", &reconnect_max, "sensitive")
    .sync_create()
    .build();
  let reconnect_group = adw::PreferencesGroup::builder().title("Auto reconnect").build();
  reconnect_group.add(&auto_reconnect);
  reconnect_group.add(&reconnect_delay);
  reconnect_group.add(&reconnect_max);

  // Application
  let close_to_tray = switch(
    "Close to tray",
    "Keep running in the system tray when the window is closed",
    s.close_to_tray,
  );
  let start_minimized = switch("Start minimized", "Start hidden in the system tray", s.start_minimized);
  let application = adw::PreferencesGroup::builder().title("Application").build();
  application.add(&close_to_tray);
  application.add(&start_minimized);

  let page_vpn = adw::PreferencesPage::builder()
    .title("VPN")
    .icon_name("network-vpn-symbolic")
    .build();
  page_vpn.add(&connection);
  page_vpn.add(&reconnect_group);
  page_vpn.add(&tunnel);
  page_vpn.add(&identity);

  let page_app = adw::PreferencesPage::builder()
    .title("General")
    .icon_name("preferences-system-symbolic")
    .build();
  page_app.add(&application);
  page_app.add(&advanced);

  let dialog = adw::PreferencesDialog::new();
  dialog.add(&page_vpn);
  dialog.add(&page_app);

  dialog.connect_closed(move |_| {
    on_close(Settings {
      as_gateway: as_gateway.is_active(),
      auto_gateway: auto_gateway.is_active(),
      use_default_browser: browser.is_active(),
      hip: hip.is_active(),
      ignore_tls_errors: ignore_tls.is_active(),
      fix_openssl: fix_openssl.is_active(),
      no_dtls: no_dtls.is_active(),
      disable_ipv6: ipv4_only.is_active(),
      mtu: mtu.value() as u32,
      reconnect_timeout: reconnect.value() as u32,
      os: ClientOs::from_index(os.selected()),
      client_version: client_version.text().trim().to_owned(),
      gpclient_path: gpclient_path.text().trim().to_owned(),
      extra_args: extra_args.text().trim().to_owned(),
      close_to_tray: close_to_tray.is_active(),
      auto_reconnect: auto_reconnect.is_active(),
      reconnect_delay: reconnect_delay.value() as u32,
      reconnect_max_attempts: reconnect_max.value() as u32,
      start_minimized: start_minimized.is_active(),
      ..s.clone()
    });
  });

  dialog.present(Some(parent));
}
