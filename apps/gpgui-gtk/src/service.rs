//! Client for the privileged `gpservice`, speaking the same protocol as the
//! proprietary gpgui:
//!
//! 1. `pkexec gpservice --desktop-credentials` (upstream polkit action
//!    `com.yuezk.gpgui.service` allows active local users without a password).
//! 2. Read a `SessionCredential` frame from the service's stdout. Keep its stdin
//!    open for the GUI's lifetime: EOF revokes the session, stops the VPN and
//!    exits the service.
//! 3. WebSocket to `ws://127.0.0.1:<port>/ws` (port from /var/run/gpservice.lock),
//!    plaintext prelude, Noise NNpsk0 handshake, `Attach`, then encrypted
//!    `Request`/`Reply` and `Event` messages.
//!
//! A dropped WebSocket does not stop a tunnel; we re-attach with the same credential.

use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};

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
  io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
  net::TcpStream,
  process::{Child, ChildStdin, Command},
  sync::oneshot,
};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, protocol::WebSocketConfig},
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::vpn::LogLevel;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// gpservice must be the same release as the gpapi we were built with.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum ServiceEvent {
  Log(LogLevel, String),
  /// Attached; carries the service's current VPN state.
  Ready(VpnState),
  State(VpnState),
  /// The service is gone (exited, refused, or pkexec was cancelled).
  Stopped(String),
}

pub enum ServiceCommand {
  Connect(Box<ConnectRequest>, oneshot::Sender<anyhow::Result<()>>),
  Disconnect,
}

pub struct ServiceHandle {
  tx: tokio::sync::mpsc::UnboundedSender<ServiceCommand>,
  pub pid: Option<u32>,
}

impl ServiceHandle {
  pub fn send(&self, cmd: ServiceCommand) -> bool {
    self.tx.send(cmd).is_ok()
  }

  pub fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<ServiceCommand> {
    self.tx.clone()
  }

  pub fn is_alive(&self) -> bool {
    !self.tx.is_closed()
  }
}

pub fn resolve_gpservice(configured: &str) -> anyhow::Result<PathBuf> {
  let configured = configured.trim();
  if !configured.is_empty() {
    let path = PathBuf::from(configured);
    ensure!(path.is_absolute(), "gpservice path must be absolute: {configured}");
    ensure!(path.is_file(), "gpservice not found at {configured}");
    return Ok(path);
  }
  ["/usr/bin/gpservice", "/usr/local/bin/gpservice"]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
    .context("gpservice was not found. Install GlobalProtect-openconnect or set its path in Preferences")
}

/// Starts gpservice and the client loop on `rt`. Dropping the handle (or the
/// GUI exiting) closes the service's stdin, which disconnects and stops it.
pub fn start(
  rt: &tokio::runtime::Handle,
  gpservice: PathBuf,
  events: async_channel::Sender<ServiceEvent>,
) -> anyhow::Result<ServiceHandle> {
  let _guard = rt.enter();
  let mut child = Command::new("pkexec")
    .arg("--user")
    .arg("root")
    .arg(&gpservice)
    .arg("--desktop-credentials")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(false)
    .spawn()
    .context("Failed to start pkexec")?;
  let pid = child.id();
  let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

  if let Some(stderr) = child.stderr.take() {
    let events = events.clone();
    rt.spawn(async move {
      let mut lines = BufReader::new(stderr).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end();
        if !line.is_empty() {
          let level = if line.contains("ERROR") {
            LogLevel::Error
          } else if line.contains("WARN") {
            LogLevel::Warn
          } else {
            LogLevel::Debug
          };
          let _ = events
            .send(ServiceEvent::Log(level, format!("[gpservice] {line}")))
            .await;
        }
      }
    });
  }

  rt.spawn(async move {
    let reason = match run(&mut child, rx, &events).await {
      Ok(()) => "Service stopped".to_owned(),
      Err(err) => format!("{err:#}"),
    };
    let status = child.wait().await;
    let reason = match status.ok().and_then(|s| s.code()) {
      Some(126) => "Authorization for gpservice was cancelled".to_owned(),
      Some(127) => "Not authorized to run gpservice (polkit)".to_owned(),
      _ => reason,
    };
    let _ = events.send(ServiceEvent::Stopped(reason)).await;
  });

  Ok(ServiceHandle { tx, pid })
}

async fn read_credential(reader: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Option<SessionCredential>> {
  // Same framing as gpapi's private desktop_launcher::read_credential.
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
  child: &mut Child,
  mut commands: tokio::sync::mpsc::UnboundedReceiver<ServiceCommand>,
  events: &async_channel::Sender<ServiceEvent>,
) -> anyhow::Result<()> {
  // Held until we return: closing it tells gpservice to disconnect and exit.
  let _control: ChildStdin = child.stdin.take().context("Service input pipe is unavailable")?;
  let mut output = child.stdout.take().context("Service credential pipe is unavailable")?;
  let log = |level, msg: String| {
    let _ = events.try_send(ServiceEvent::Log(level, msg));
  };

  let credential = tokio::select! {
    result = read_credential(&mut output) => result?.context("gpservice exited before issuing a session credential")?,
    _ = child.wait() => bail!("gpservice exited during startup"),
  };
  ensure!(
    credential.product_version() == PRODUCT_VERSION,
    "gpservice {} does not match this GUI ({PRODUCT_VERSION}); install matching versions",
    credential.product_version()
  );
  log(LogLevel::Info, "gpservice started".into());

  let mut failures = 0u32;
  loop {
    let endpoint = gpapi::utils::endpoint::ws_endpoint().await?;
    match attach(&credential, &endpoint).await {
      Ok((mut socket, mut noise, state)) => {
        failures = 0;
        let _ = events.send(ServiceEvent::Ready(state)).await;
        match session(&mut socket, &mut noise, &mut commands, events).await {
          Ok(SessionEnd::GuiClosed) => return Ok(()),
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
        ensure!(failures < 20, "Cannot attach to gpservice: {err:#}");
        log(LogLevel::Warn, format!("Attach failed: {err:#}"));
      }
    }
    tokio::select! {
      _ = tokio::time::sleep(Duration::from_millis(500)) => {}
      _ = child.wait() => bail!("gpservice exited"),
    }
  }
}

/// An error after which re-attaching cannot help.
#[derive(Debug)]
struct Fatal(String);

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
  .context("Timed out connecting to gpservice")??;

  let prelude = ClientPrelude {
    product_version: credential.product_version().to_owned(),
    hip_protocol_version: HIP_PROTOCOL_VERSION,
    service_instance_id: credential.service_instance_id(),
    session_id: credential.session_id(),
  };
  socket.send(Message::Binary(prelude.encode()?.into())).await?;
  match ServerPrelude::decode(&receive_binary(&mut socket).await?)? {
    ServerPrelude::Ready => {}
    ServerPrelude::Rejected(reason) => return Err(Fatal(format!("gpservice rejected the GUI: {reason:?}")).into()),
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
    _ => bail!("gpservice did not attach the GUI session"),
  }
}

async fn receive_binary(socket: &mut Socket) -> anyhow::Result<Vec<u8>> {
  let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
    .await
    .context("gpservice handshake timed out")?;
  match next {
    Some(Ok(Message::Binary(data))) => Ok(data.to_vec()),
    Some(Ok(Message::Close(frame))) => bail!("gpservice closed the connection: {frame:?}"),
    Some(Ok(_)) => bail!("gpservice sent an unexpected handshake frame"),
    Some(Err(err)) => Err(err.into()),
    None => bail!("gpservice closed during handshake"),
  }
}

enum SessionEnd {
  GuiClosed,
}

async fn session(
  socket: &mut Socket,
  noise: &mut NoiseTransport,
  commands: &mut tokio::sync::mpsc::UnboundedReceiver<ServiceCommand>,
  events: &async_channel::Sender<ServiceEvent>,
) -> anyhow::Result<SessionEnd> {
  let mut pending: HashMap<Uuid, Option<oneshot::Sender<anyhow::Result<()>>>> = HashMap::new();
  let mut revision: Option<u64> = None;
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
        if let Err(err) = socket.send(Message::Binary(encrypted.into())).await {
          if let Some(reply) = reply {
            let _ = reply.send(Err(anyhow::anyhow!("Lost connection to gpservice")));
          }
          return Err(err.into());
        }
        pending.insert(id, reply);
      }
      frame = socket.next() => {
        let Some(frame) = frame else { bail!("gpservice closed the connection") };
        match frame? {
          Message::Binary(data) => match noise.decrypt::<ServerMessage>(&data)? {
            ServerMessage::Reply { id, result } => {
              if let Some(Some(reply)) = pending.remove(&id) {
                let _ = reply.send(match result {
                  ServiceResult::Accepted => Ok(()),
                  ServiceResult::Rejected(rejection) => Err(anyhow::anyhow!("{rejection:?}")),
                  other => Err(anyhow::anyhow!("Unexpected service reply: {other:?}")),
                });
              }
            }
            ServerMessage::Event { revision: rev, event } => {
              if revision.is_some_and(|last| rev != last + 1) {
                let resync = noise.encrypt(&ClientMessage::Resync { last_event_revision: revision.unwrap_or(0) })?;
                socket.send(Message::Binary(resync.into())).await?;
              }
              revision = Some(rev);
              match event {
                WsEvent::VpnState(state) => { let _ = events.send(ServiceEvent::State(state)).await; }
                WsEvent::VpnEnv(env) => { let _ = events.send(ServiceEvent::State(env.vpn_state)).await; }
                _ => {}
              }
            }
            ServerMessage::Snapshot { current_revision, snapshot } => {
              revision = Some(current_revision);
              let _ = events.send(ServiceEvent::State(snapshot.vpn_state)).await;
            }
            ServerMessage::Attached { .. } => bail!("Duplicate attach from gpservice"),
            ServerMessage::TunnelStatistics { .. } => {}
          },
          Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
          Message::Close(frame) => {
            // 4003 SessionReplaced / 4002 Unauthorized / 4011 ServiceRestarted cannot be fixed by re-attaching.
            let code = frame.as_ref().map(|f| u16::from(f.code));
            if matches!(code, Some(4002 | 4003 | 4011)) {
              return Err(Fatal(format!("gpservice ended the session ({frame:?})")).into());
            }
            bail!("gpservice closed the connection: {frame:?}");
          }
          _ => {}
        }
      }
    }
  }
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

  #[test]
  fn configured_gpservice_path_must_be_absolute_and_exist() {
    assert!(resolve_gpservice("gpservice").is_err());
    assert!(resolve_gpservice("/nonexistent/gpservice").is_err());
    assert_eq!(resolve_gpservice("/bin/sh").unwrap(), PathBuf::from("/bin/sh"));
  }
}
