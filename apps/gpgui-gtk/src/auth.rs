//! Portal/gateway authentication in the user session, producing the
//! `ConnectRequest` that gpservice (root) turns into a tunnel.
//!
//! This mirrors gpclient's flow (apps/gpclient/src/connect/{mod,gateway,credential}.rs):
//! portal prelogin → credential (password or SAML via gpauth) → portal config →
//! gateway prelogin/login (portal cookie first, then gateway credential; MFA) →
//! single-member tunnel `ConnectionPlan`. Internal (non-tunnel) gateways are not
//! supported yet.

use anyhow::{Context, anyhow, bail};
use gpapi::{
  credential::{AuthCookieCredential, Credential, PasswordCredential},
  gateway::{
    Gateway, GatewayLogin, GatewayLoginClient, GatewayLoginContext, GatewayLoginProtocolError, GatewaySelection,
  },
  gp_params::GpParams,
  hip::HipSource,
  os_profile::{ClientOs, OsProfile},
  portal::{Prelogin, PreloginOptions, prelogin, retrieve_config},
  process::auth_launcher::SamlAuthLauncher,
  service::{request::ConnectRequest, vpn_state::ConnectInfo},
  session::{AuthenticatedGateway, ConnectionPlan, GatewayAuthentication, GatewaySessions, SessionMode},
};
use tokio_util::sync::CancellationToken;

use crate::config::Settings;

/// What the UI collected.
#[derive(Clone)]
pub struct AuthInput {
  pub portal: String,
  pub gateway: Option<String>,
  pub username: Option<String>,
  pub password: Option<String>,
  pub settings: Settings,
}

/// Callbacks into the UI (implemented over channels by the caller).
pub struct AuthUi {
  pub log: Box<dyn Fn(String) + Send + Sync>,
  /// Ask the user for an MFA/OTP code; `None` = cancelled.
  pub ask_otp: Box<dyn Fn(String) -> async_channel::Receiver<Option<String>> + Send + Sync>,
}

impl AuthUi {
  fn log(&self, msg: impl Into<String>) {
    (self.log)(msg.into())
  }

  async fn otp(&self, message: &str) -> anyhow::Result<String> {
    let rx = (self.ask_otp)(message.to_owned());
    match rx.recv().await {
      Ok(Some(code)) => Ok(code),
      _ => Err(gpapi::auth::AuthenticationCancelled.into()),
    }
  }
}

/// Result of a successful authentication. `sessions` must be relinquished once
/// gpservice accepts the request, or logged out otherwise.
pub struct Authenticated {
  pub request: ConnectRequest,
  pub sessions: GatewaySessions,
  pub gateway_name: String,
}

pub fn os_profile(settings: &Settings) -> OsProfile {
  let mut builder = OsProfile::builder(ClientOs::from(settings.os.as_arg()));
  let version = settings.client_version.trim();
  if !version.is_empty() {
    builder = builder.client_version(version);
  }
  builder.build()
}

struct Ctx<'a> {
  input: &'a AuthInput,
  ui: &'a AuthUi,
  cancel: &'a CancellationToken,
  os_profile: OsProfile,
  params: GpParams,
  sessions: GatewaySessions,
}

pub async fn authenticate(input: &AuthInput, ui: &AuthUi, cancel: &CancellationToken) -> anyhow::Result<Authenticated> {
  let os_profile = os_profile(&input.settings);
  let params = GpParams::builder(os_profile.clone())
    .ignore_tls_errors(input.settings.ignore_tls_errors)
    .build();
  let mut ctx = Ctx {
    input,
    ui,
    cancel,
    os_profile,
    params,
    sessions: GatewaySessions::default(),
  };
  let result = if input.settings.as_gateway {
    ctx.direct_gateway().await
  } else {
    ctx.via_portal().await
  };
  match result {
    Ok((request, gateway_name)) => Ok(Authenticated {
      request,
      sessions: ctx.sessions,
      gateway_name,
    }),
    Err(err) => {
      ctx.sessions.logout().await;
      Err(err)
    }
  }
}

impl Ctx<'_> {
  fn check_cancelled(&self) -> anyhow::Result<()> {
    if self.cancel.is_cancelled() {
      Err(gpapi::auth::AuthenticationCancelled.into())
    } else {
      Ok(())
    }
  }

  async fn prelogin(&self, server: &str, is_gateway: bool) -> anyhow::Result<Prelogin> {
    let mut params = self.params.clone();
    params.set_is_gateway(is_gateway);
    let options = PreloginOptions::default()
      .external_browser_requested(self.input.settings.use_default_browser)
      .gateway_external_browser_allowed(true);
    tokio::select! {
      biased;
      _ = self.cancel.cancelled() => Err(gpapi::auth::AuthenticationCancelled.into()),
      result = prelogin(server, &params, options) => result,
    }
  }

  async fn obtain_credential(&self, prelogin: &Prelogin, server: &str) -> anyhow::Result<Credential> {
    match prelogin {
      Prelogin::Saml(saml) => {
        self.ui.log(format!(
          "SAML sign-in required for {server}; opening the authentication window…"
        ));
        let browser = (self.input.settings.use_default_browser && saml.support_default_browser()).then_some("default");
        let os_profile = self.os_profile.clone();
        let cred = SamlAuthLauncher::new(server)
          .gateway(prelogin.is_gateway())
          .saml_request(saml.saml_request())
          .os_profile(&os_profile)
          .ignore_tls_errors(self.input.settings.ignore_tls_errors)
          .fix_openssl(self.input.settings.fix_openssl)
          .browser(browser)
          .cancellation(self.cancel)
          .launch()
          .await?;
        self.check_cancelled()?;
        Ok(cred)
      }
      Prelogin::Standard(standard) => {
        let user = self
          .input
          .username
          .as_deref()
          .with_context(|| format!("{} is required ({server})", standard.label_username()))?;
        let password = self
          .input
          .password
          .as_deref()
          .with_context(|| format!("{} is required ({server})", standard.label_password()))?;
        Ok(PasswordCredential::new(user, password).into())
      }
    }
  }

  async fn login_gateway(
    &mut self,
    server: &str,
    cred: &Credential,
    context: Option<&GatewayLoginContext>,
  ) -> anyhow::Result<GatewayAuthentication> {
    let mut params = self.params.clone();
    params.set_is_gateway(true);
    params.prepare_client_identity()?;
    let mut client = GatewayLoginClient::new(server, params)?;
    loop {
      self.check_cancelled()?;
      match client.login(cred, context, self.cancel).await? {
        GatewayLogin::Authenticated(auth) => {
          client.register_session(
            Gateway::new(server.to_owned(), server.to_owned()),
            auth.clone(),
            &mut self.sessions,
          );
          return Ok(auth);
        }
        GatewayLogin::Mfa(message, input_str) => {
          self.ui.log(format!("Gateway {server} requests a one-time code"));
          let otp = self.ui.otp(&message).await?;
          client.respond_mfa(&input_str, &otp);
        }
      }
    }
  }

  /// gateway.rs `authenticate_gateway`: portal cookie first, gateway credential as fallback.
  async fn authenticate_gateway(
    &mut self,
    gateway: &Gateway,
    portal_cookie: &AuthCookieCredential,
    context: &GatewayLoginContext,
  ) -> anyhow::Result<GatewayAuthentication> {
    let server = gateway.server().to_owned();
    let gateway_prelogin = self.prelogin(&server, true).await?;
    if portal_cookie.can_authenticate_gateway() {
      let cred: Credential = portal_cookie.into();
      match self.login_gateway(&server, &cred, Some(context)).await {
        Ok(auth) => return Ok(auth),
        Err(err) => {
          self.check_cancelled()?;
          if err.downcast_ref::<GatewayLoginProtocolError>().is_some() {
            return Err(err);
          }
          self.ui.log(format!(
            "Portal cookie rejected by {server}; signing in to the gateway: {err}"
          ));
        }
      }
    }
    let cred = self.obtain_credential(&gateway_prelogin, &server).await?;
    self.login_gateway(&server, &cred, Some(context)).await
  }

  async fn via_portal(&mut self) -> anyhow::Result<(ConnectRequest, String)> {
    let portal = self.input.portal.clone();
    self.ui.log(format!("Portal prelogin: {portal}"));
    let portal_prelogin = self.prelogin(&portal, false).await?;
    let cred = self.obtain_credential(&portal_prelogin, &portal).await?;

    self.ui.log("Retrieving portal configuration…");
    let mut params = self.params.clone();
    params.set_is_gateway(false);
    let mut config = tokio::select! {
      biased;
      _ = self.cancel.cancelled() => return Err(gpapi::auth::AuthenticationCancelled.into()),
      result = retrieve_config(&portal, &cred, &params, self.cancel) => result?,
    };
    config.sort_gateways(portal_prelogin.region());
    let portal_cookie = match cred.password() {
      Some(password) => config.auth_cookie().clone().with_password(password),
      None => config.auth_cookie().clone(),
    };
    let all: Vec<Gateway> = config.gateways().into_iter().cloned().collect();
    if all.is_empty() {
      bail!("The portal returned no gateways");
    }

    let (candidates, selection) = match self.input.gateway.as_deref() {
      Some(wanted) => {
        let gateway = config
          .find_gateway(wanted)
          .cloned()
          .ok_or_else(|| anyhow!("Gateway '{wanted}' is not offered by the portal"))?;
        (vec![gateway], GatewaySelection::Manual)
      }
      None if self.input.settings.auto_gateway || all.len() == 1 => (all.clone(), GatewaySelection::Auto),
      None => (vec![all[0].clone()], GatewaySelection::Auto),
    };

    let mut last_error = None;
    for gateway in candidates {
      self.check_cancelled()?;
      self.ui.log(format!("Signing in to gateway {gateway}"));
      let context = GatewayLoginContext::new(&gateway, selection).with_connect_method(config.connect_method());
      match self.authenticate_gateway(&gateway, &portal_cookie, &context).await {
        Ok(auth) => {
          if auth.mode() != SessionMode::Tunnel {
            self.sessions.logout_gateway(gateway.server()).await;
            last_error = Some(anyhow!(
              "Gateway {gateway} offered an internal (non-tunnel) session, which is not supported yet"
            ));
            continue;
          }
          let plan = ConnectionPlan::new(
            vec![AuthenticatedGateway {
              gateway: gateway.clone(),
              authentication: auth,
              binding: None,
            }],
            vec![],
            config.internal_session_policy().unwrap_or_default(),
          )?;
          let info = ConnectInfo::new(portal.clone(), gateway.clone(), all.clone());
          let request = self
            .request(info, plan)
            .with_allow_extend_session(config.allow_extend_session().unwrap_or(false));
          return Ok((request, gateway.name().to_owned()));
        }
        Err(err) => {
          if err.downcast_ref::<gpapi::auth::AuthenticationCancelled>().is_some() {
            return Err(err);
          }
          self.ui.log(format!("Gateway {gateway} failed: {err:#}"));
          self.sessions.logout_gateway(gateway.server()).await;
          last_error = Some(err);
        }
      }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("No gateway could be authenticated")))
  }

  async fn direct_gateway(&mut self) -> anyhow::Result<(ConnectRequest, String)> {
    let server = self.input.portal.clone();
    self.ui.log(format!("Gateway prelogin: {server}"));
    let prelogin = self.prelogin(&server, true).await?;
    let cred = self.obtain_credential(&prelogin, &server).await?;
    let auth = self.login_gateway(&server, &cred, None).await?;
    if auth.mode() != SessionMode::Tunnel {
      bail!("The gateway offered an internal (non-tunnel) session, which is not supported yet");
    }
    let gateway = Gateway::new(server.clone(), server.clone());
    let plan = ConnectionPlan::new(
      vec![AuthenticatedGateway {
        gateway: gateway.clone(),
        authentication: auth,
        binding: None,
      }],
      vec![],
      Default::default(),
    )?;
    let info = ConnectInfo::new(server.clone(), gateway.clone(), vec![gateway]);
    Ok((self.request(info, plan), server))
  }

  fn request(&self, info: ConnectInfo, plan: ConnectionPlan) -> ConnectRequest {
    let s = &self.input.settings;
    ConnectRequest::new(info, plan)
      .with_os_profile(&self.os_profile)
      .with_hip_source(if s.hip {
        HipSource::Generated
      } else {
        HipSource::Disabled
      })
      .with_ignore_tls_errors(s.ignore_tls_errors)
      .with_reconnect_timeout(s.reconnect_timeout)
      .with_mtu(s.mtu)
      .with_no_dtls(s.no_dtls)
      .with_disable_ipv6(s.disable_ipv6)
  }
}
