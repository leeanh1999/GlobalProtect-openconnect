//! Small custom-drawn dialogs: verification code, confirmations and About.
//! They are owned, non-modal windows (the owner is disabled while shown), so
//! the message loop never nests.

use gpdesktop::{controller::Command, procs::VpnProcess};
use tokio::sync::oneshot;
use windows::Win32::Foundation::HWND;

use super::{
  Effect,
  gfx::{Align, Canvas, Font, Svg, measure, rect},
  scene::{ButtonStyle, Id, Kind, Surface},
  theme::Theme,
};

pub const WIDTH: f32 = 420.0;
const MARGIN: f32 = 24.0;
const ID_BUTTON: Id = 10; // ..13
const ID_INPUT: Id = 20;

pub enum DialogKind {
  /// MFA / one-time code.
  Otp {
    reply: Option<oneshot::Sender<Option<String>>>,
  },
  StopOthers {
    reply: Option<oneshot::Sender<bool>>,
  },
  ServiceStopped,
  Quit,
  About,
}

pub struct Button {
  pub label: &'static str,
  pub style: ButtonStyle,
}

pub struct Dialog {
  pub s: Surface,
  pub kind: DialogKind,
  pub title: String,
  pub body: String,
  buttons: Vec<Button>,
  input: Option<usize>,
  pub height: f32,
}

impl Dialog {
  pub fn otp(hwnd: HWND, theme: Theme, message: String, reply: oneshot::Sender<Option<String>>) -> Self {
    Self::build(
      hwnd,
      theme,
      DialogKind::Otp { reply: Some(reply) },
      "Verification code".into(),
      message,
      vec![
        Button {
          label: "Cancel",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: "Continue",
          style: ButtonStyle::Primary,
        },
      ],
      true,
    )
  }

  pub fn stop_others(
    hwnd: HWND,
    theme: Theme,
    processes: &[VpnProcess],
    then_connect: bool,
    reply: oneshot::Sender<bool>,
  ) -> Self {
    let list = processes
      .iter()
      .map(VpnProcess::describe)
      .collect::<Vec<_>>()
      .join("\n");
    Self::build(
      hwnd,
      theme,
      DialogKind::StopOthers { reply: Some(reply) },
      "Other VPN processes are running".into(),
      format!("They can hold the network or the tunnel and block a new connection:\n\n{list}"),
      vec![
        Button {
          label: "Cancel",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: if then_connect {
            "Stop & Connect"
          } else {
            "Stop Processes"
          },
          style: ButtonStyle::Danger,
        },
      ],
      false,
    )
  }

  pub fn service_stopped(hwnd: HWND, theme: Theme, reason: String) -> Self {
    Self::build(
      hwnd,
      theme,
      DialogKind::ServiceStopped,
      "VPN service is not running".into(),
      format!("{reason}. Start it now? Windows will ask for administrator permission."),
      vec![
        Button {
          label: "Cancel",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: "Start Service",
          style: ButtonStyle::Primary,
        },
      ],
      false,
    )
  }

  pub fn quit(hwnd: HWND, theme: Theme) -> Self {
    Self::build(
      hwnd,
      theme,
      DialogKind::Quit,
      "Quit GlobalProtect VPN?".into(),
      "The VPN is still running. It stays connected after the app quits unless you disconnect it.".into(),
      vec![
        Button {
          label: "Cancel",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: "Keep VPN",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: "Disconnect",
          style: ButtonStyle::Danger,
        },
      ],
      false,
    )
  }

  pub fn about(hwnd: HWND, theme: Theme) -> Self {
    Self::build(
      hwnd,
      theme,
      DialogKind::About,
      "GlobalProtect VPN".into(),
      format!(
        "Version {}\n\nOpen-source Windows frontend for GlobalProtect-openconnect.\nLicensed under the GNU GPL v3.",
        env!("CARGO_PKG_VERSION")
      ),
      vec![
        Button {
          label: "Website",
          style: ButtonStyle::Secondary,
        },
        Button {
          label: "Close",
          style: ButtonStyle::Primary,
        },
      ],
      false,
    )
  }

  fn build(
    hwnd: HWND,
    theme: Theme,
    kind: DialogKind,
    title: String,
    body: String,
    buttons: Vec<Button>,
    with_input: bool,
  ) -> Self {
    let mut s = Surface::new(hwnd, theme);
    let input = with_input.then(|| s.add_edit(ID_INPUT, "Code", false, false));
    let mut dialog = Self {
      s,
      kind,
      title,
      body,
      buttons,
      input,
      height: 0.0,
    };
    dialog.height = dialog.measure_height();
    dialog
  }

  fn header_h(&self) -> f32 {
    if matches!(self.kind, DialogKind::About) {
      76.0
    } else {
      0.0
    }
  }

  fn body_top(&self) -> f32 {
    MARGIN + self.header_h() + 34.0
  }

  fn body_h(&self) -> f32 {
    measure(&self.body, Font::body(14.0), WIDTH - 2.0 * MARGIN, true)
      .1
      .ceil()
  }

  fn measure_height(&self) -> f32 {
    let mut h = self.body_top() + self.body_h() + 20.0;
    if self.input.is_some() {
      h += 40.0 + 20.0;
    }
    h + 40.0 + MARGIN
  }

  pub fn layout(&mut self) {
    self.s.hide_all();
    let mut y = self.body_top() + self.body_h() + 20.0;
    if let Some(input) = self.input {
      self.s.place_field(
        ID_INPUT,
        input,
        rect(MARGIN, y, WIDTH - 2.0 * MARGIN, 40.0),
        false,
        true,
      );
      y += 60.0;
    }
    let n = self.buttons.len() as f32;
    let bw = if n > 2.0 { 112.0 } else { 132.0 };
    for (i, button) in self.buttons.iter().enumerate() {
      let x = WIDTH - MARGIN - (n - i as f32) * bw - (n - 1.0 - i as f32) * 10.0;
      self.s.put(
        ID_BUTTON + i as Id,
        rect(x, y, bw, 40.0),
        Kind::Button {
          label: button.label.into(),
          style: button.style,
        },
        true,
      );
    }
    self.s.invalidate();
  }

  /// Button `index` (0-based, left to right) was chosen; `None` = closed/Esc.
  pub fn choose(&mut self, index: Option<usize>) -> Vec<Effect> {
    let last = self.buttons.len().saturating_sub(1);
    let mut effects = vec![Effect::CloseDialog(self.s.hwnd)];
    match &mut self.kind {
      DialogKind::Otp { reply } => {
        let code = (index == Some(last)).then(|| {
          self
            .input
            .map(|i| self.s.edit_text(i).trim().to_owned())
            .unwrap_or_default()
        });
        if let Some(reply) = reply.take() {
          let _ = reply.send(code);
        }
      }
      DialogKind::StopOthers { reply } => {
        if let Some(reply) = reply.take() {
          let _ = reply.send(index == Some(last));
        }
      }
      DialogKind::ServiceStopped => {
        if index == Some(last) {
          effects.push(Effect::Send(Command::StartService));
        }
      }
      DialogKind::Quit => match index {
        Some(1) => effects.push(Effect::Send(Command::Quit { disconnect: false })),
        Some(2) => effects.push(Effect::Send(Command::Quit { disconnect: true })),
        _ => {}
      },
      DialogKind::About => {
        if index == Some(0) {
          effects.push(Effect::OpenUrl(
            "https://github.com/leeanh1999/GlobalProtect-openconnect".into(),
          ));
        }
      }
    }
    effects
  }

  pub fn on_click(&mut self, id: Id) -> Vec<Effect> {
    if id >= ID_BUTTON && id < ID_BUTTON + self.buttons.len() as Id {
      self.choose(Some((id - ID_BUTTON) as usize))
    } else {
      vec![]
    }
  }

  /// Enter picks the last (default) button.
  pub fn on_enter(&mut self) -> Vec<Effect> {
    let last = self.buttons.len().saturating_sub(1);
    if matches!(self.kind, DialogKind::Quit | DialogKind::StopOthers { .. }) {
      // Destructive defaults stay explicit: Enter = Cancel.
      return self.choose(Some(0));
    }
    self.choose(Some(last))
  }

  pub fn paint(&self, c: &Canvas) {
    let t = &self.s.theme;
    c.clear(t.bg);
    let mut top = MARGIN;
    if matches!(self.kind, DialogKind::About) {
      c.svg(Svg::App, rect(MARGIN, top, 64.0, 64.0), self.s.scale);
      top += self.header_h();
    }
    c.text(
      &self.title,
      rect(MARGIN, top, WIDTH - 2.0 * MARGIN, 28.0),
      Font::strong(18.0),
      t.text.d2d(),
      Align::Leading,
      true,
      false,
    );
    c.text(
      &self.body,
      rect(MARGIN, self.body_top(), WIDTH - 2.0 * MARGIN, self.body_h() + 4.0),
      Font::body(14.0),
      t.text_muted.d2d(),
      Align::Leading,
      false,
      true,
    );
    self.s.paint_nodes(c);
  }
}

impl Drop for Dialog {
  fn drop(&mut self) {
    // Closing without a choice cancels pending prompts.
    match &mut self.kind {
      DialogKind::Otp { reply } => {
        if let Some(reply) = reply.take() {
          let _ = reply.send(None);
        }
      }
      DialogKind::StopOthers { reply } => {
        if let Some(reply) = reply.take() {
          let _ = reply.send(false);
        }
      }
      _ => {}
    }
  }
}
