//! Client for the privileged `gpservice`, speaking the same protocol as the
//! proprietary GUI:
//!
//! 1. Bootstrap a `SessionCredential` and a *lease* that keeps it valid:
//!    - Windows ([`windows`]): connect to the `GPConnect` service's admission
//!      pipe, send `GPC1`, read the credential frame and keep the pipe open.
//!    - Linux/BSD ([`unix`]): `pkexec gpservice --desktop-credentials`, read the
//!      frame from its stdout and keep its stdin open.
//! 2. WebSocket to `ws://127.0.0.1:<port>/ws` (port from the service lock file),
//!    plaintext prelude, Noise NNpsk0 handshake, `Attach`, then encrypted
//!    `Request`/`Reply` and `Event` messages.
//!
//! A dropped WebSocket does not stop a tunnel; we re-attach with the same credential.
//! Losing the lease ends the session (and, on Linux, stops the private service).

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use gpapi::service::{
  event::WsEvent,
  request::{ConnectRequest, DisconnectRequest, WsRequest},
  transport::{
    ClientMessage, ClientPrelude, HIP_PROTOCOL_VERSION, MAX_CREDENTIAL_FRAME, MAX_WS_MESSAGE, NoiseInitiator,
    NoiseTransport, ServerMessage, ServerPrelude, ServiceResult, SessionCredential,
  },
  vpn_state::VpnState,
};
use tokio::{
  io::{AsyncRead, AsyncReadExt},
  net::TcpStream,
  sync::{mpsc, oneshot},
};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, protocol::WebSocketConfig},
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::LogLevel;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// gpservice must be the same release as the gpapi we were built with.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum ServiceEvent {
  Log(LogLevel, String),
  /// Attached; carries our session id and the service's current VPN state.
  Ready {
    session_id: Uuid,
    state: VpnState,
  },
  State(VpnState),
  /// The service connection is gone for good (service stopped, refused, lease lost).
  Stopped(String),
}

pub enum ServiceCommand {
  Connect(Box<ConnectRequest>, oneshot::Sender<anyhow::Result<()>>),
  Disconnect,
}

pub struct ServiceHandle {
  tx: mpsc::UnboundedSender<ServiceCommand>,
  /// PID of a service process we own (Linux pkexec child); `None` for a system service.
  pub pid: Option<u32>,
}

impl ServiceHandle {
  pub fn send(&self, cmd: ServiceCommand) -> bool {
    self.tx.send(cmd).is_ok()
  }

  pub fn sender(&self) -> mpsc::UnboundedSender<ServiceCommand> {
    self.tx.clone()
  }

  pub fn is_alive(&self) -> bool {
    !self.tx.is_closed()
  }
}

/// Keeps the session credential valid while held.
pub(crate) enum Lease {
  #[cfg(windows)]
  Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
  #[cfg(unix)]
  Child {
    child: tokio::process::Child,
    _stdin: tokio::process::ChildStdin,
    _stdout: tokio::process::ChildStdout,
  },
  /// Tests only: never ends.
  #[cfg(test)]
  Forever,
}

impl Lease {
  /// Completes when the lease is lost; returns a human-readable reason. Cancel safe.
  pub(crate) async fn lost(&mut self) -> String {
    match self {
      #[cfg(windows)]
      Lease::Pipe(pipe) => {
        let mut byte = [0u8; 1];
        match pipe.read(&mut byte).await {
          Ok(0) => "The VPN service ended this session".to_owned(),
          Ok(_) => "The VPN service sent an unexpected lease message".to_owned(),
          Err(err) => format!("Lost the VPN service session: {err}"),
        }
      }
      #[cfg(unix)]
      Lease::Child { child, .. } => match child.wait().await {
        Ok(status) => match status.code() {
          Some(126) => "Authorization for gpservice was cancelled".to_owned(),
          Some(127) => "Not authorized to run gpservice (polkit)".to_owned(),
          _ => format!("gpservice exited ({status})"),
        },
        Err(err) => format!("gpservice exited: {err}"),
      },
      #[cfg(test)]
      Lease::Forever => std::future::pending().await,
    }
  }
}

pub(crate) struct Bootstrap {
  pub credential: SessionCredential,
  pub lease: Lease,
}

/// Credential delivery still in progress (a pipe or child process is open).
pub(crate) type PendingBoot = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Bootstrap>> + Send>>;

/// Starts the platform bootstrap and the client loop on `rt`. Dropping every
/// sender of the returned handle ends the session (releasing the lease).
/// `gpservice_path` is only used on Linux/BSD (empty = auto-detect).
pub fn start(
  rt: &tokio::runtime::Handle,
  gpservice_path: &str,
  events: mpsc::UnboundedSender<ServiceEvent>,
) -> anyhow::Result<ServiceHandle> {
  let _guard = rt.enter();
  #[cfg(windows)]
  let (pid, boot) = {
    let _ = gpservice_path;
    windows::bootstrap()?
  };
  #[cfg(unix)]
  let (pid, boot) = unix::bootstrap(gpservice_path, &events)?;
  let (tx, rx) = mpsc::unbounded_channel();
  rt.spawn(async move {
    let result = async { run(boot.await?, rx, &events).await }.await;
    let reason = match result {
      Ok(()) => "Service session closed".to_owned(),
      Err(err) => format!("{err:#}"),
    };
    let _ = events.send(ServiceEvent::Stopped(reason));
  });
  Ok(ServiceHandle { tx, pid })
}

/// Reads a length-prefixed credential frame (same framing as gpapi's desktop launcher).
pub(crate) async fn read_credential(
  reader: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Option<SessionCredential>> {
  let mut header = [0; 2];
  if reader.read(&mut header[..1]).await? == 0 {
    return Ok(None);
  }
  reader.read_exact(&mut header[1..]).await?;
  let length = usize::from(u16::from_be_bytes(header));
  ensure!(
    length != 0 && length + 2 <= MAX_CREDENTIAL_FRAME,
    "Invalid service credential length"
  );
  let mut frame = Zeroizing::new(vec![0; length + 2]);
  frame[..2].copy_from_slice(&header);
  reader.read_exact(&mut frame[2..]).await?;
  Ok(Some(SessionCredential::decode_frame(&frame)?))
}

async fn run(
  boot: Bootstrap,
  mut commands: mpsc::UnboundedReceiver<ServiceCommand>,
  events: &mpsc::UnboundedSender<ServiceEvent>,
) -> anyhow::Result<()> {
  let Bootstrap { credential, mut lease } = boot;
  ensure!(
    credential.product_version() == PRODUCT_VERSION,
    "The VPN service ({}) does not match this app ({PRODUCT_VERSION}); install matching versions",
    credential.product_version()
  );
  let log = |level, msg: String| {
    let _ = events.send(ServiceEvent::Log(level, msg));
  };

  let mut failures = 0u32;
  loop {
    let attached = tokio::select! {
      reason = lease.lost() => bail!(reason),
      attached = async {
        let endpoint = gpapi::utils::endpoint::ws_endpoint().await?;
        attach(&credential, &endpoint).await
      } => attached,
    };
    match attached {
      Ok((mut socket, mut noise, state)) => {
        failures = 0;
        let _ = events.send(ServiceEvent::Ready {
          session_id: credential.session_id(),
          state,
        });
        let ended = tokio::select! {
          reason = lease.lost() => bail!(reason),
          ended = session(&mut socket, &mut noise, &mut commands, events) => ended,
        };
        match ended {
          Ok(SessionEnd::GuiClosed) => {
            let _ = socket.close(None).await;
            return Ok(());
          }
          Err(err) if err.is::<Fatal>() => return Err(err),
          Err(err) => log(
            LogLevel::Warn,
            format!("Service connection lost: {err:#}; re-attaching"),
          ),
        }
      }
      Err(err) if err.is::<Fatal>() => return Err(err),
      Err(err) => {
        failures += 1;
        ensure!(failures < 20, "Cannot attach to the VPN service: {err:#}");
        log(LogLevel::Warn, format!("Attach failed: {err:#}"));
      }
    }
    tokio::select! {
      _ = tokio::time::sleep(Duration::from_millis(500)) => {}
      reason = lease.lost() => bail!(reason),
    }
  }
}

/// An error after which re-attaching cannot help.
#[derive(Debug)]
pub(crate) struct Fatal(pub String);

impl std::fmt::Display for Fatal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for Fatal {}

async fn attach(credential: &SessionCredential, endpoint: &str) -> anyhow::Result<(Socket, NoiseTransport, VpnState)> {
  let config = WebSocketConfig::default()
    .max_message_size(Some(MAX_WS_MESSAGE))
    .max_frame_size(Some(MAX_WS_MESSAGE));
  let (mut socket, _) = tokio::time::timeout(
    Duration::from_secs(5),
    connect_async_with_config(endpoint, Some(config), false),
  )
  .await
  .context("Timed out connecting to the VPN service")??;

  let prelude = ClientPrelude {
    product_version: credential.product_version().to_owned(),
    hip_protocol_version: HIP_PROTOCOL_VERSION,
    service_instance_id: credential.service_instance_id(),
    session_id: credential.session_id(),
  };
  socket.send(Message::Binary(prelude.encode()?.into())).await?;
  match ServerPrelude::decode(&receive_binary(&mut socket).await?)? {
    ServerPrelude::Ready => {}
    ServerPrelude::Rejected(reason) => {
      return Err(Fatal(format!("The VPN service rejected this app: {reason:?}")).into());
    }
  }

  let mut initiator = NoiseInitiator::new(
    credential.product_version(),
    credential.service_instance_id(),
    credential.session_id(),
    credential.secret(),
  )?;
  socket.send(Message::Binary(initiator.write_first()?.into())).await?;
  let mut noise = initiator.read_response(&receive_binary(&mut socket).await?)?;
  socket
    .send(Message::Binary(
      noise
        .encrypt(&ClientMessage::Attach {
          last_event_revision: None,
        })?
        .into(),
    ))
    .await?;
  match noise.decrypt::<ServerMessage>(&receive_binary(&mut socket).await?)? {
    ServerMessage::Attached { snapshot, .. } => Ok((socket, noise, snapshot.vpn_state)),
    _ => bail!("The VPN service did not attach this session"),
  }
}

async fn receive_binary(socket: &mut Socket) -> anyhow::Result<Vec<u8>> {
  let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
    .await
    .context("VPN service handshake timed out")?;
  match next {
    Some(Ok(Message::Binary(data))) => Ok(data.to_vec()),
    Some(Ok(Message::Close(frame))) => bail!("The VPN service closed the connection: {frame:?}"),
    Some(Ok(_)) => bail!("The VPN service sent an unexpected handshake frame"),
    Some(Err(err)) => Err(err.into()),
    None => bail!("The VPN service closed during handshake"),
  }
}

enum SessionEnd {
  GuiClosed,
}

fn reply_result(result: ServiceResult) -> anyhow::Result<()> {
  match result {
    ServiceResult::Accepted => Ok(()),
    ServiceResult::Rejected(rejection) => Err(anyhow::anyhow!("{} ({:?})", rejection.message(), rejection.code())),
    other => Err(anyhow::anyhow!("Unexpected service reply: {other:?}")),
  }
}

async fn session(
  socket: &mut Socket,
  noise: &mut NoiseTransport,
  commands: &mut mpsc::UnboundedReceiver<ServiceCommand>,
  events: &mpsc::UnboundedSender<ServiceEvent>,
) -> anyhow::Result<SessionEnd> {
  let mut pending: HashMap<Uuid, Option<oneshot::Sender<anyhow::Result<()>>>> = HashMap::new();
  let mut revision: Option<u64> = None;
  let result = async {
    loop {
      tokio::select! {
        cmd = commands.recv() => {
          let Some(cmd) = cmd else { return Ok(SessionEnd::GuiClosed) };
          let id = Uuid::new_v4();
          let (request, reply) = match cmd {
            ServiceCommand::Connect(request, reply) => (WsRequest::Connect(request), Some(reply)),
            ServiceCommand::Disconnect => (WsRequest::Disconnect(DisconnectRequest), None),
          };
          let encrypted = noise.encrypt(&ClientMessage::Request { id, request })?;
          pending.insert(id, reply);
          socket.send(Message::Binary(encrypted.into())).await?;
        }
        frame = socket.next() => {
          let Some(frame) = frame else { bail!("The VPN service closed the connection") };
          match frame? {
            Message::Binary(data) => match noise.decrypt::<ServerMessage>(&data)? {
              ServerMessage::Reply { id, result } => {
                if let Some(Some(reply)) = pending.remove(&id) {
                  let _ = reply.send(reply_result(result));
                }
              }
              ServerMessage::Event { revision: rev, event } => {
                if revision.is_some_and(|last| rev != last + 1) {
                  let resync = noise.encrypt(&ClientMessage::Resync { last_event_revision: revision.unwrap_or(0) })?;
                  socket.send(Message::Binary(resync.into())).await?;
                }
                revision = Some(rev);
                match event {
                  WsEvent::VpnState(state) => { let _ = events.send(ServiceEvent::State(state)); }
                  WsEvent::VpnEnv(env) => { let _ = events.send(ServiceEvent::State(env.vpn_state)); }
                  _ => {}
                }
              }
              ServerMessage::Snapshot { current_revision, snapshot } => {
                revision = Some(current_revision);
                let _ = events.send(ServiceEvent::State(snapshot.vpn_state));
              }
              ServerMessage::Attached { .. } => bail!("Duplicate attach from the VPN service"),
              ServerMessage::TunnelStatistics { .. } => {}
            },
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Close(frame) => {
              // 4002 Unauthorized / 4003 SessionReplaced / 4011 ServiceRestarted cannot be fixed by re-attaching.
              let code = frame.as_ref().map(|f| u16::from(f.code));
              if matches!(code, Some(4002 | 4003 | 4011)) {
                return Err(Fatal(format!("The VPN service ended the session ({frame:?})")).into());
              }
              bail!("The VPN service closed the connection: {frame:?}");
            }
            _ => {}
          }
        }
      }
    }
  }
  .await;
  // Requests in flight have no reply coming on this connection.
  for (_, reply) in pending.drain() {
    if let Some(reply) = reply {
      let _ = reply.send(Err(anyhow::anyhow!("Lost connection to the VPN service")));
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn reads_credential_frames_like_the_upstream_launcher() {
    let issued = SessionCredential::generate(Uuid::new_v4(), PRODUCT_VERSION).unwrap();
    let frame = issued.encode_frame().unwrap();
    let mut stream: &[u8] = &frame;
    let read = read_credential(&mut stream).await.unwrap().unwrap();
    assert_eq!(read.session_id(), issued.session_id());
    assert_eq!(read.secret(), issued.secret());
    assert_eq!(read.product_version(), PRODUCT_VERSION);

    // EOF before a frame = service exited cleanly.
    let mut empty: &[u8] = &[];
    assert!(read_credential(&mut empty).await.unwrap().is_none());
    // Oversized length prefix is rejected.
    let mut bad: &[u8] = &[0xff, 0xff, 0];
    assert!(read_credential(&mut bad).await.is_err());
  }

  #[tokio::test]
  async fn version_mismatch_is_reported_before_attaching() {
    let credential = SessionCredential::generate(Uuid::new_v4(), "0.0.0-other").unwrap();
    let (_tx, rx) = mpsc::unbounded_channel();
    let (events, _events_rx) = mpsc::unbounded_channel();
    let boot = Bootstrap {
      credential,
      lease: Lease::Forever,
    };
    let err = run(boot, rx, &events).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not match"), "{err:#}");
  }
}
