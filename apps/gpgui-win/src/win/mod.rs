//! Win32 shell: window classes, message loop, the app state shared by all
//! windows, menus, and the bridge from controller events to the UI thread.

mod autostart;
mod dialog;
mod gfx;
mod log_window;
mod main_window;
mod prefs;
mod scene;
mod theme;
mod tray;

use std::{
  cell::RefCell,
  collections::VecDeque,
  process::ExitCode,
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use gpdesktop::{
  config::{ClientOs, Settings},
  controller::{self, ActionKind, Command, ControllerHandle, StatusView, UiEvent},
  state::{StatusIcon, VpnStatus},
};
use windows::{
  Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
      BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
      DeleteObject, EndPaint, GetDC, GetMonitorInfoW, HDC, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
      MonitorFromWindow, PAINTSTRUCT, ReleaseDC, SelectObject, SetBkColor, SetTextColor,
    },
    Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
    System::{
      Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
      DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
      LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
      Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
      Ole::CF_UNICODETEXT,
      Threading::CreateMutexW,
    },
    UI::{
      HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
      Input::KeyboardAndMouse::{
        GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT,
        TrackMouseEvent, VK_CONTROL,
      },
      Shell::{NIN_SELECT, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, ShellExecuteW},
      WindowsAndMessaging::*,
    },
  },
  core::{HSTRING, PCSTR, PCWSTR, w},
};

use self::{
  dialog::Dialog,
  gfx::{Svg, svg_icon},
  log_window::{LogBuffer, LogWindow},
  main_window::{MainWindow, Page},
  scene::Surface,
  theme::Theme,
};

const CLASS_MAIN: PCWSTR = w!("GlobalProtectVPN.Main");
const CLASS_WINDOW: PCWSTR = w!("GlobalProtectVPN.Window");
const WM_APP_UI: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_ACTIVATE: u32 = WM_APP + 3;
const TIMER_TICK: usize = 1;
// Not exported by the `windows` crate features we enable.
const WM_MOUSELEAVE: u32 = 0x02A3;
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;

/// Work that must run after the app state is released (menus, new windows,
/// anything that can re-enter the window procedure).
pub enum Effect {
  Send(Command),
  MainMenu,
  ModeMenu,
  PortalMenu,
  OsMenu,
  TrayMenu,
  OpenPrefs,
  ClosePrefs,
  /// New settings and the "Start with Windows" choice.
  SavePrefs(Box<Settings>, bool),
  OpenLog,
  CloseLog,
  CopyLog,
  ClearLog,
  OpenDialog(DialogSpec),
  CloseDialog(HWND),
  OpenUrl(String),
  ShowMain,
  ToggleMain,
  RequestQuit,
}

pub enum DialogSpec {
  Otp(String, tokio::sync::oneshot::Sender<Option<String>>),
  StopOthers(
    Vec<gpdesktop::procs::VpnProcess>,
    bool,
    tokio::sync::oneshot::Sender<bool>,
  ),
  ServiceStopped(String),
  Quit,
  About,
}

struct App {
  _rt: tokio::runtime::Runtime,
  controller: ControllerHandle,
  queue: Arc<Mutex<VecDeque<UiEvent>>>,
  theme: Theme,
  main: MainWindow,
  log: Option<LogWindow>,
  dialogs: Vec<Dialog>,
  log_buf: LogBuffer,
  tray: tray::Tray,
  settings: Settings,
  status: StatusView,
  taskbar_created: u32,
  /// Screenshot mode: ignore controller events.
  demo: bool,
  /// When the main window last lost activation (tray click toggling).
  main_deactivated: Option<Instant>,
}

thread_local! {
  static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state unless it is already borrowed (re-entrant message).
fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
  APP.with(|cell| match cell.try_borrow_mut() {
    Ok(mut guard) => guard.as_mut().map(f),
    Err(_) => None,
  })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
  Main,
  Log,
  Dialog(usize),
}

impl App {
  fn target(&self, hwnd: HWND) -> Option<Target> {
    if self.main.s.hwnd == hwnd {
      return Some(Target::Main);
    }
    if self.log.as_ref().is_some_and(|l| l.s.hwnd == hwnd) {
      return Some(Target::Log);
    }
    self.dialogs.iter().position(|d| d.s.hwnd == hwnd).map(Target::Dialog)
  }

  fn surface(&mut self, target: Target) -> &mut Surface {
    match target {
      Target::Main => &mut self.main.s,
      Target::Log => &mut self.log.as_mut().unwrap().s,
      Target::Dialog(i) => &mut self.dialogs[i].s,
    }
  }

  fn layout(&mut self, target: Target) {
    match target {
      Target::Main => self.main.layout(),
      Target::Log => self.log.as_mut().unwrap().layout(),
      Target::Dialog(i) => self.dialogs[i].layout(),
    }
  }

  fn click(&mut self, target: Target, id: scene::Id) -> Vec<Effect> {
    match target {
      Target::Main => self.main.on_click(id),
      Target::Log => self.log.as_mut().unwrap().on_click(id),
      Target::Dialog(i) => self.dialogs[i].on_click(id),
    }
  }

  fn paint(&mut self, target: Target, hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    unsafe { BeginPaint(hwnd, &mut ps) };
    // WM_SIZE may have been skipped while the state was busy: keep the target in sync.
    let mut rc = RECT::default();
    unsafe {
      let _ = GetClientRect(hwnd, &mut rc);
    }
    let (w, h) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
    // Take the render target out so the page can be borrowed immutably while drawing.
    let (mut rt, scale) = {
      let s = self.surface(target);
      s.target.resize(w, h);
      (std::mem::replace(&mut s.target, gfx::WindowTarget::new()), s.scale)
    };
    let result = rt.draw(hwnd, scale, |c| match target {
      Target::Main => self.main.paint(c),
      Target::Log => self.log.as_ref().unwrap().paint(c),
      Target::Dialog(i) => self.dialogs[i].paint(c),
    });
    if let Err(err) = result {
      log::warn!("Paint failed: {err:#}");
    }
    self.surface(target).target = rt;
    unsafe {
      let _ = EndPaint(hwnd, &ps);
    }
  }

  fn set_theme(&mut self, theme: Theme) {
    if theme == self.theme {
      return;
    }
    self.theme = theme;
    self.main.s.apply_theme(theme);
    if let Some(l) = &mut self.log {
      l.s.apply_theme(theme);
      unsafe {
        let _ = windows::Win32::UI::Controls::SetWindowTheme(
          l.view,
          if theme.dark {
            w!("DarkMode_Explorer")
          } else {
            w!("Explorer")
          },
          PCWSTR::null(),
        );
      }
    }
    for d in &mut self.dialogs {
      d.s.apply_theme(theme);
    }
    self.tray.refresh_icons();
  }

  fn main_visible(&self) -> bool {
    unsafe { IsWindowVisible(self.main.s.hwnd).as_bool() && !IsIconic(self.main.s.hwnd).as_bool() }
  }

  fn tunnel_active(&self) -> bool {
    self.status.status != VpnStatus::Disconnected
  }

  // ---- controller events ---------------------------------------------------------------

  fn drain_ui(&mut self) -> Vec<Effect> {
    let events: Vec<UiEvent> = self.queue.lock().unwrap().drain(..).collect();
    let mut effects = Vec::new();
    for event in events {
      if self.demo {
        continue;
      }
      match event {
        UiEvent::Status(view) => {
          self.tray.update(&view);
          let title = HSTRING::from(format!("GlobalProtect VPN — {}", view.title));
          unsafe {
            let _ = SetWindowTextW(self.main.s.hwnd, &title);
          }
          self.status = view.clone();
          self.main.set_status(view);
        }
        UiEvent::Log(level, message) => {
          let line = self.log_buf.push(level, &message);
          if let Some(log) = &self.log {
            log.append(&line);
          }
        }
        UiEvent::Toast(message) => {
          if self.main_visible() {
            self.main.toast(message);
          } else {
            self.tray.balloon("GlobalProtect VPN", &message);
          }
        }
        UiEvent::AskOtp { message, reply } => {
          effects.push(Effect::ShowMain);
          effects.push(Effect::OpenDialog(DialogSpec::Otp(message, reply)));
        }
        UiEvent::ConfirmStopOthers {
          processes,
          then_connect,
          reply,
        } => {
          effects.push(Effect::ShowMain);
          effects.push(Effect::OpenDialog(DialogSpec::StopOthers(
            processes,
            then_connect,
            reply,
          )));
        }
        UiEvent::ServiceNotRunning(reason) => {
          effects.push(Effect::ShowMain);
          effects.push(Effect::OpenDialog(DialogSpec::ServiceStopped(reason)));
        }
        UiEvent::Settings(settings) => {
          self.settings = (*settings).clone();
          self.main.set_settings(*settings);
        }
        UiEvent::Exit => {
          self.tray.remove();
          unsafe { PostQuitMessage(0) };
        }
      }
    }
    effects
  }

  // ---- window procedure --------------------------------------------------------------------

  fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<(LRESULT, Vec<Effect>)> {
    if msg == self.taskbar_created && msg != 0 {
      self.tray.readd();
      return Some((LRESULT(0), vec![]));
    }
    let target = self.target(hwnd)?;
    let done = |effects| Some((LRESULT(0), effects));
    match msg {
      WM_PAINT => {
        self.paint(target, hwnd);
        done(vec![])
      }
      WM_SIZE => {
        let (w, h) = ((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32);
        self.surface(target).target.resize(w, h);
        if target == Target::Log {
          self.layout(target);
        }
        self.surface(target).invalidate();
        done(vec![])
      }
      WM_DPICHANGED => {
        let suggested = unsafe { *(lparam.0 as *const RECT) };
        if self.surface(target).refresh_scale() {
          self.layout(target);
        }
        let (hwnd_copy, r) = (hwnd, suggested);
        unsafe {
          let _ = SetWindowPos(
            hwnd_copy,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
          );
        }
        if target == Target::Main {
          self.tray.refresh_icons();
        }
        done(vec![])
      }
      WM_MOUSEMOVE => {
        let s = self.surface(target);
        let (x, y) = mouse_dips(lparam, s.scale);
        let hover = s.hit(x, y);
        if !s.tracking_mouse {
          let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
          };
          unsafe {
            let _ = TrackMouseEvent(&mut tme);
          }
          s.tracking_mouse = true;
        }
        if hover != s.hover {
          s.hover = hover;
          s.invalidate();
        }
        done(vec![])
      }
      WM_MOUSELEAVE => {
        let s = self.surface(target);
        s.tracking_mouse = false;
        if s.hover.take().is_some() {
          s.invalidate();
        }
        done(vec![])
      }
      WM_LBUTTONDOWN => {
        let s = self.surface(target);
        let (x, y) = mouse_dips(lparam, s.scale);
        s.pressed = s.hit(x, y);
        if s.pressed.is_some() {
          unsafe {
            SetCapture(hwnd);
          }
        } else {
          // Clicking empty space takes focus away from text boxes.
          unsafe {
            let _ = SetFocus(Some(hwnd));
          }
        }
        s.invalidate();
        done(vec![])
      }
      WM_LBUTTONUP => {
        let s = self.surface(target);
        let (x, y) = mouse_dips(lparam, s.scale);
        let pressed = s.pressed.take();
        unsafe {
          let _ = ReleaseCapture();
        }
        s.invalidate();
        let effects = match pressed {
          Some(id) if s.hit(x, y) == Some(id) => self.click(target, id),
          _ => vec![],
        };
        done(effects)
      }
      WM_SETCURSOR => {
        let s = self.surface(target);
        if (lparam.0 & 0xFFFF) as u32 == HTCLIENT && s.hover.is_some() {
          unsafe {
            if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
              SetCursor(Some(cursor));
            }
          }
          return Some((LRESULT(1), vec![]));
        }
        None
      }
      WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
        let s = self.surface(target);
        let child = HWND(lparam.0 as *mut _);
        let enabled = unsafe { IsWindowEnabled(child).as_bool() };
        let (text, bg, brush) = s.edit_colors(enabled || target == Target::Log);
        let hdc = HDC(wparam.0 as *mut _);
        unsafe {
          SetTextColor(hdc, text);
          SetBkColor(hdc, bg);
        }
        Some((LRESULT(brush.0 as isize), vec![]))
      }
      WM_COMMAND => {
        let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
        let id = (wparam.0 & 0xFFFF) as i32;
        if code == EN_SETFOCUS || code == EN_KILLFOCUS {
          self.surface(target).invalidate();
          return done(vec![]);
        }
        if lparam.0 != 0 && code != 0 {
          return None;
        }
        let effects = match (target, id) {
          (Target::Main, 1) => self.main.on_enter(),
          (Target::Main, 2) => self.main.on_escape(),
          (Target::Log, 2) => vec![Effect::CloseLog],
          (Target::Dialog(i), 1) => self.dialogs[i].on_enter(),
          (Target::Dialog(i), 2) => self.dialogs[i].choose(None),
          _ => vec![],
        };
        done(effects)
      }
      WM_TIMER if target == Target::Main => {
        self.main.tick();
        done(vec![])
      }
      WM_APP_UI => {
        let effects = self.drain_ui();
        done(effects)
      }
      WM_APP_ACTIVATE => done(vec![Effect::ShowMain]),
      WM_ACTIVATE if target == Target::Main => {
        if wparam.0 & 0xFFFF == WA_INACTIVE as usize {
          self.main_deactivated = Some(Instant::now());
        }
        None
      }
      WM_APP_TRAY => {
        let event = (lparam.0 & 0xFFFF) as u32;
        let effects = match event {
          // With NOTIFYICON_VERSION_4 one click also sends WM_LBUTTONDOWN/UP;
          // reacting to those as well toggled the window twice (open, then hide).
          NIN_SELECT | NIN_KEYSELECT => vec![Effect::ToggleMain],
          WM_CONTEXTMENU => vec![Effect::TrayMenu],
          _ => vec![],
        };
        done(effects)
      }
      WM_SETTINGCHANGE => {
        let theme = Theme::system();
        self.set_theme(theme);
        None
      }
      WM_CLOSE => {
        let effects = match target {
          Target::Main if self.settings.close_to_tray => {
            unsafe {
              let _ = ShowWindow(hwnd, SW_HIDE);
            }
            vec![]
          }
          Target::Main => vec![Effect::RequestQuit],
          Target::Log => vec![Effect::CloseLog],
          Target::Dialog(i) => self.dialogs[i].choose(None),
        };
        done(effects)
      }
      _ => None,
    }
  }
}

fn mouse_dips(lparam: LPARAM, scale: f32) -> (f32, f32) {
  let x = (lparam.0 & 0xFFFF) as i16 as f32;
  let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
  (x / scale, y / scale)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
  if msg == WM_ERASEBKGND {
    return LRESULT(1);
  }
  match with_app(|app| app.handle(hwnd, msg, wparam, lparam)).flatten() {
    Some((result, effects)) => {
      run_effects(effects);
      result
    }
    None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
  }
}

// ---- effects ------------------------------------------------------------------------------------

fn run_effects(effects: Vec<Effect>) {
  for effect in effects {
    run_effect(effect);
  }
}

/// Shows a popup menu at client DIP position `(x, y)` of `hwnd`; returns the chosen id.
fn popup_menu(hwnd: HWND, x: f32, y: f32, scale: f32, items: &[(u32, String, bool, bool)]) -> u32 {
  unsafe {
    let Ok(menu) = CreatePopupMenu() else { return 0 };
    for (id, label, checked, enabled) in items {
      if *id == 0 {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        continue;
      }
      let mut flags = MF_STRING;
      if *checked {
        flags |= MF_CHECKED;
      }
      if !*enabled {
        flags |= MF_GRAYED;
      }
      let _ = AppendMenuW(menu, flags, *id as usize, &HSTRING::from(label.as_str()));
    }
    let mut pt = POINT {
      x: (x * scale).round() as i32,
      y: (y * scale).round() as i32,
    };
    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenuEx(
      menu,
      (TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_NONOTIFY).0,
      pt.x,
      pt.y,
      hwnd,
      None,
    );
    let _ = DestroyMenu(menu);
    chosen.0 as u32
  }
}

fn screen_menu(hwnd: HWND, items: &[(u32, String, bool, bool)]) -> u32 {
  let mut pt = POINT::default();
  unsafe {
    let _ = GetCursorPos(&mut pt);
    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
  }
  popup_menu(hwnd, pt.x as f32, pt.y as f32, 1.0, items)
}

fn run_effect(effect: Effect) {
  match effect {
    Effect::Send(command) => {
      with_app(|app| app.controller.send(command));
    }
    Effect::MainMenu => {
      let Some((hwnd, (x, y), scale, auto)) = with_app(|app| {
        (
          app.main.s.hwnd,
          app.main.menu_anchor(&Effect::MainMenu),
          app.main.s.scale,
          app.settings.auto_reconnect,
        )
      }) else {
        return;
      };
      let items = vec![
        (1, "Show Log\tCtrl+L".to_owned(), false, true),
        (2, "Preferences…".to_owned(), false, true),
        (0, String::new(), false, true),
        (3, "Reconnect Automatically".to_owned(), auto, true),
        (4, "Stop Other VPN Processes…".to_owned(), false, true),
        (0, String::new(), false, true),
        (5, "About GlobalProtect VPN".to_owned(), false, true),
        (6, "Quit".to_owned(), false, true),
      ];
      match popup_menu(hwnd, x, y, scale, &items) {
        1 => run_effect(Effect::OpenLog),
        2 => run_effect(Effect::OpenPrefs),
        3 => run_effect(Effect::Send(Command::SetAutoReconnect(!auto))),
        4 => run_effect(Effect::Send(Command::StopOthers)),
        5 => run_effect(Effect::OpenDialog(DialogSpec::About)),
        6 => run_effect(Effect::RequestQuit),
        _ => {}
      }
    }
    Effect::ModeMenu => {
      let Some((hwnd, (x, y), scale, settings)) = with_app(|app| {
        (
          app.main.s.hwnd,
          app.main.menu_anchor(&Effect::ModeMenu),
          app.main.s.scale,
          app.settings.clone(),
        )
      }) else {
        return;
      };
      let items = vec![
        (1, "Portal".to_owned(), !settings.as_gateway, true),
        (2, "Gateway".to_owned(), settings.as_gateway, true),
      ];
      let as_gateway = match popup_menu(hwnd, x, y, scale, &items) {
        1 => false,
        2 => true,
        _ => return,
      };
      if as_gateway != settings.as_gateway {
        let updated = Settings { as_gateway, ..settings };
        with_app(|app| {
          app.settings = updated.clone();
          app.main.set_settings(updated.clone());
          app.controller.send(Command::UpdateSettings(Box::new(updated)));
        });
      }
    }
    Effect::PortalMenu => {
      let Some((hwnd, (x, y), scale, recent)) = with_app(|app| {
        (
          app.main.s.hwnd,
          app.main.menu_anchor(&Effect::PortalMenu),
          app.main.s.scale,
          app.settings.recent_portals.clone(),
        )
      }) else {
        return;
      };
      let items: Vec<_> = recent
        .iter()
        .enumerate()
        .map(|(i, p)| (100 + i as u32, p.clone(), false, true))
        .collect();
      let chosen = popup_menu(hwnd, x, y, scale, &items);
      if let Some(portal) = chosen.checked_sub(100).and_then(|i| recent.get(i as usize)) {
        with_app(|app| app.main.set_portal(portal));
      }
    }
    Effect::OsMenu => {
      let Some((hwnd, (x, y), scale, current)) = with_app(|app| {
        let m = &app.main;
        (m.s.hwnd, m.prefs.os_anchor(&m.s), m.s.scale, m.prefs.os())
      }) else {
        return;
      };
      let items: Vec<_> = ClientOs::ALL
        .iter()
        .map(|os| (os.index() + 1, os.as_arg().to_owned(), *os == current, true))
        .collect();
      let chosen = popup_menu(hwnd, x, y, scale, &items);
      if chosen > 0 {
        with_app(|app| {
          app.main.prefs.set_os(ClientOs::from_index(chosen - 1));
          app.main.layout();
          app.main.s.invalidate();
        });
      }
    }
    Effect::TrayMenu => {
      let Some((hwnd, view)) = with_app(|app| (app.main.s.hwnd, app.status.clone())) else {
        return;
      };
      let action = match view.action {
        ActionKind::Connect => "Connect",
        ActionKind::Cancel => "Cancel",
        ActionKind::Disconnect => "Disconnect",
      };
      let items = vec![
        (1, format!("Status: {}", view.title), false, false),
        (0, String::new(), false, true),
        (2, action.to_owned(), false, view.action_enabled),
        (3, "Show Window".to_owned(), false, true),
        (0, String::new(), false, true),
        (4, "Quit".to_owned(), false, true),
      ];
      match screen_menu(hwnd, &items) {
        2 => {
          let effects = with_app(|app| app.main.primary_action()).unwrap_or_default();
          run_effects(effects);
        }
        3 => run_effect(Effect::ShowMain),
        4 => run_effect(Effect::RequestQuit),
        _ => {}
      }
    }
    Effect::OpenPrefs => {
      // Settings is a page of the main window, not a separate window.
      with_app(|app| {
        if app.main.page != Page::Settings {
          let settings = app.settings.clone();
          app.main.open_settings(settings);
        }
      });
      run_effect(Effect::ShowMain);
    }
    Effect::ClosePrefs => {
      with_app(|app| app.main.close_settings());
    }
    Effect::SavePrefs(settings, start_with_windows) => {
      if start_with_windows != autostart::is_enabled()
        && let Err(err) = autostart::set(start_with_windows)
      {
        log::warn!("{err:#}");
        with_app(|app| app.main.toast(format!("{err:#}")));
      }
      with_app(|app| {
        let mut merged = (*settings).clone();
        merged.profile = app.settings.profile.clone();
        merged.recent_portals = app.settings.recent_portals.clone();
        app.settings = merged.clone();
        app.main.set_settings(merged);
        app.controller.send(Command::UpdateSettings(settings));
      });
      run_effect(Effect::ClosePrefs);
    }
    Effect::OpenLog => {
      if let Some(Some(hwnd)) = with_app(|app| app.log.as_ref().map(|l| l.s.hwnd)) {
        activate(hwnd);
        return;
      }
      let Some((owner, theme)) = with_app(|app| (app.main.s.hwnd, app.theme)) else {
        return;
      };
      let Some(hwnd) = create_window(
        CLASS_WINDOW,
        "Connection Log",
        Some(owner),
        log_window::WIDTH,
        log_window::HEIGHT,
        true,
      ) else {
        return;
      };
      with_app(|app| {
        let window = LogWindow::new(hwnd, theme, &app.log_buf);
        app.log = Some(window);
      });
      show(hwnd);
    }
    Effect::CloseLog => {
      if let Some(Some(window)) = with_app(|app| app.log.take()) {
        destroy(window.s.hwnd);
      }
    }
    Effect::CopyLog => {
      if let Some((hwnd, text)) = with_app(|app| (app.main.s.hwnd, app.log_buf.text())) {
        copy_to_clipboard(hwnd, &text);
        with_app(|app| app.main.toast("Log copied".into()));
      }
    }
    Effect::ClearLog => {
      with_app(|app| {
        app.log_buf.clear();
        if let Some(l) = &app.log {
          l.set_text("");
        }
      });
    }
    Effect::OpenDialog(spec) => open_dialog(spec),
    Effect::CloseDialog(hwnd) => {
      if let Some(Some(dialog)) = with_app(|app| {
        let i = app.dialogs.iter().position(|d| d.s.hwnd == hwnd)?;
        Some(app.dialogs.remove(i))
      }) {
        destroy(dialog.s.hwnd);
        drop(dialog);
      }
    }
    Effect::OpenUrl(url) => unsafe {
      ShellExecuteW(
        None,
        w!("open"),
        &HSTRING::from(url),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
      );
    },
    Effect::ShowMain => {
      if let Some((hwnd, visible)) = with_app(|app| (app.main.s.hwnd, app.main_visible())) {
        if !visible {
          place_near_tray(hwnd);
        }
        activate(hwnd);
      }
    }
    Effect::ToggleMain => {
      let Some((hwnd, visible, deactivated)) =
        with_app(|app| (app.main.s.hwnd, app.main_visible(), app.main_deactivated))
      else {
        return;
      };
      // Clicking the tray icon activates the taskbar first, so "was in front"
      // means: foreground now, or lost activation a moment ago.
      let in_front = unsafe { GetForegroundWindow() } == hwnd
        || deactivated.is_some_and(|at| at.elapsed() < Duration::from_millis(400));
      if visible && in_front {
        unsafe {
          let _ = ShowWindow(hwnd, SW_HIDE);
        }
      } else {
        if !visible {
          place_near_tray(hwnd);
        }
        activate(hwnd);
      }
    }
    Effect::RequestQuit => {
      let active = with_app(|app| app.tunnel_active()).unwrap_or(false);
      if active {
        run_effect(Effect::ShowMain);
        open_dialog(DialogSpec::Quit);
      } else {
        run_effect(Effect::Send(Command::Quit { disconnect: false }));
      }
    }
  }
}

fn open_dialog(spec: DialogSpec) {
  let Some((owner, theme)) = with_app(|app| (app.main.s.hwnd, app.theme)) else {
    return;
  };
  let title = match &spec {
    DialogSpec::Otp(..) => "Verification code",
    DialogSpec::StopOthers(..) => "Other VPN processes",
    DialogSpec::ServiceStopped(..) => "VPN service",
    DialogSpec::Quit => "Quit",
    DialogSpec::About => "About",
  };
  let Some(hwnd) = create_window(CLASS_WINDOW, title, Some(owner), dialog::WIDTH, 200.0, false) else {
    return;
  };
  let mut dialog = match spec {
    DialogSpec::Otp(message, reply) => Dialog::otp(hwnd, theme, message, reply),
    DialogSpec::StopOthers(processes, then_connect, reply) => {
      Dialog::stop_others(hwnd, theme, &processes, then_connect, reply)
    }
    DialogSpec::ServiceStopped(reason) => Dialog::service_stopped(hwnd, theme, reason),
    DialogSpec::Quit => Dialog::quit(hwnd, theme),
    DialogSpec::About => Dialog::about(hwnd, theme),
  };
  dialog.layout();
  resize_client(hwnd, dialog::WIDTH, dialog.height, Some(owner));
  let focus = dialog.s.edits.first().map(|e| e.hwnd);
  with_app(|app| app.dialogs.push(dialog));
  show(hwnd);
  if let Some(edit) = focus {
    unsafe {
      let _ = SetFocus(Some(edit));
    }
  }
}

/// Moves the main window to the screen corner next to the notification area
/// (bottom-right with a bottom taskbar; follows top/left/right taskbars and
/// the monitor that shows the tray icon).
fn place_near_tray(hwnd: HWND) {
  unsafe {
    let id = NOTIFYICONIDENTIFIER {
      cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
      hWnd: hwnd,
      uID: tray::TRAY_ID,
      ..Default::default()
    };
    let icon = Shell_NotifyIconGetRect(&id).ok();
    let anchor = match icon {
      Some(r) => POINT {
        x: (r.left + r.right) / 2,
        y: (r.top + r.bottom) / 2,
      },
      // Icon hidden or not added yet: primary monitor, bottom-right.
      None => POINT {
        x: GetSystemMetrics(SM_CXSCREEN) - 1,
        y: GetSystemMetrics(SM_CYSCREEN) - 1,
      },
    };
    let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
      cbSize: std::mem::size_of::<MONITORINFO>() as u32,
      ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
      return;
    }
    let (mon, work) = (info.rcMonitor, info.rcWork);
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
      return;
    }
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let margin = (12.0 * scene::window_scale(hwnd)).round() as i32;
    let right = anchor.x >= (mon.left + mon.right) / 2;
    let bottom = anchor.y >= (mon.top + mon.bottom) / 2;
    let x = if right {
      work.right - w - margin
    } else {
      work.left + margin
    };
    let y = if bottom {
      work.bottom - h - margin
    } else {
      work.top + margin
    };
    let _ = SetWindowPos(
      hwnd,
      None,
      x.max(work.left),
      y.max(work.top),
      0,
      0,
      SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
  }
}

fn activate(hwnd: HWND) {
  unsafe {
    if IsIconic(hwnd).as_bool() {
      let _ = ShowWindow(hwnd, SW_RESTORE);
    } else {
      let _ = ShowWindow(hwnd, SW_SHOW);
    }
    let _ = SetForegroundWindow(hwnd);
  }
}

fn show(hwnd: HWND) {
  unsafe {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
  }
}

fn destroy(hwnd: HWND) {
  unsafe {
    let _ = DestroyWindow(hwnd);
  }
}

fn copy_to_clipboard(hwnd: HWND, text: &str) {
  let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
  unsafe {
    if OpenClipboard(Some(hwnd)).is_err() {
      return;
    }
    let _ = EmptyClipboard();
    if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
      let ptr = GlobalLock(mem) as *mut u16;
      if !ptr.is_null() {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(mem);
        let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)));
      }
    }
    let _ = CloseClipboard();
  }
}

// ---- windows ------------------------------------------------------------------------------------

fn register_classes() -> anyhow::Result<()> {
  unsafe {
    let instance = GetModuleHandleW(None)?;
    let icon = svg_icon(Svg::App, 32).ok();
    let small = svg_icon(Svg::App, GetSystemMetrics(SM_CXSMICON).max(16) as u32).ok();
    for class in [CLASS_MAIN, CLASS_WINDOW] {
      let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hIcon: icon.unwrap_or_default(),
        hIconSm: small.unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: class,
        ..Default::default()
      };
      if RegisterClassExW(&wc) == 0 {
        anyhow::bail!("RegisterClassExW failed: {}", std::io::Error::last_os_error());
      }
    }
  }
  Ok(())
}

fn window_style(resizable: bool) -> WINDOW_STYLE {
  // WS_CLIPCHILDREN: Direct2D must not paint over the native EDIT children.
  if resizable {
    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
  } else {
    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN
  }
}

/// Creates a hidden top-level window sized for `width`×`height` client DIPs.
fn create_window(
  class: PCWSTR,
  title: &str,
  owner: Option<HWND>,
  width: f32,
  height: f32,
  resizable: bool,
) -> Option<HWND> {
  let style = window_style(resizable);
  let hwnd = unsafe {
    CreateWindowExW(
      WINDOW_EX_STYLE(0),
      class,
      &HSTRING::from(title),
      style,
      CW_USEDEFAULT,
      CW_USEDEFAULT,
      100,
      100,
      owner,
      None,
      GetModuleHandleW(None).ok().map(Into::into),
      None,
    )
  }
  .map_err(|err| log::error!("CreateWindowExW: {err}"))
  .ok()?;
  resize_client(hwnd, width, height, owner);
  Some(hwnd)
}

/// Sizes `hwnd` for a client area of `width`×`height` DIPs, centred on `owner` or its monitor.
fn resize_client(hwnd: HWND, width: f32, height: f32, owner: Option<HWND>) {
  unsafe {
    let scale = scene::window_scale(hwnd);
    let dpi = GetDpiForWindow(hwnd).max(96);
    let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
    let mut rc = RECT {
      left: 0,
      top: 0,
      right: (width * scale).round() as i32,
      bottom: (height * scale).round() as i32,
    };
    let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    // Centre on a visible owner, otherwise on the monitor's work area.
    let owner = owner.filter(|o| IsWindowVisible(*o).as_bool());
    let monitor = MonitorFromWindow(owner.unwrap_or(hwnd), MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
      cbSize: std::mem::size_of::<MONITORINFO>() as u32,
      ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut info);
    let work = info.rcWork;
    let mut area = work;
    if let Some(owner) = owner {
      let _ = GetWindowRect(owner, &mut area);
    }
    let x = area.left + ((area.right - area.left) - w) / 2;
    let y = area.top + ((area.bottom - area.top) - h) / 2;
    // The main window sits in the tray corner: keep larger windows fully on screen.
    let margin = (12.0 * scale).round() as i32;
    let (x, y) = clamp_to_work(x, y, w, h, &work, margin);
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
  }
}

/// Moves a `w`×`h` window at (`x`, `y`) inside `work` (with `margin`), top-left wins if it is too large.
fn clamp_to_work(x: i32, y: i32, w: i32, h: i32, work: &RECT, margin: i32) -> (i32, i32) {
  let x = x
    .min(work.right - w - margin)
    .max(work.left + margin.min((work.right - work.left - w).max(0)));
  let y = y
    .min(work.bottom - h - margin)
    .max(work.top + margin.min((work.bottom - work.top - h).max(0)));
  (x.max(work.left), y.max(work.top))
}

// ---- screenshot mode (dev aid) ------------------------------------------------------------------

/// `GPGUI_WIN_SCREENSHOT=out.png GPGUI_WIN_DEMO=connected|connecting|error|reconnecting|log|prefs|otp|about|quit`
/// renders a window to a PNG and exits (with `GPGUI_WIN_THEME`, `GPGUI_WIN_SCALE`).
fn apply_demo(demo: &str) -> Vec<Effect> {
  let now = controller::now_unix();
  let mut view = with_app(|app| app.status.clone()).unwrap();
  view.portal = "vpn.example.com".into();
  let mut effects = vec![];
  match demo {
    "connected" => {
      view.status = VpnStatus::Connected;
      view.icon = StatusIcon::Connected;
      view.title = "Connected".into();
      view.detail = "vpn.example.com · US East".into();
      view.gateway = "US East".into();
      view.connected_at = Some(now - 260);
      view.form_locked = true;
      view.action = ActionKind::Disconnect;
    }
    "connecting" => {
      view.status = VpnStatus::Connecting;
      view.icon = StatusIcon::Connecting;
      view.title = "Connecting…".into();
      view.detail = "Signing in to vpn.example.com…".into();
      view.form_locked = true;
      view.action = ActionKind::Cancel;
    }
    "reconnecting" => {
      view.status = VpnStatus::Reconnecting;
      view.icon = StatusIcon::Connecting;
      view.title = "Reconnecting…".into();
      view.detail = "Retrying vpn.example.com in 17s (attempt 3)".into();
      view.form_locked = true;
      view.action = ActionKind::Cancel;
    }
    "error" => {
      view.icon = StatusIcon::Error;
      view.failed = true;
      view.detail = "Gateway US East failed: authentication failed, invalid username or password".into();
    }
    "log" => {
      with_app(|app| {
        for (level, line) in [
          (gpdesktop::LogLevel::Info, "──── Connecting to vpn.example.com ────"),
          (gpdesktop::LogLevel::Info, "Portal prelogin: vpn.example.com"),
          (
            gpdesktop::LogLevel::Info,
            "SAML sign-in required for vpn.example.com; opening the authentication window…",
          ),
          (gpdesktop::LogLevel::Info, "Retrieving portal configuration…"),
          (
            gpdesktop::LogLevel::Warn,
            "Portal cookie rejected by gw1.example.com; signing in to the gateway",
          ),
          (gpdesktop::LogLevel::Info, "Connected to US East"),
        ] {
          app.log_buf.push(level, line);
        }
      });
      effects.push(Effect::OpenLog);
    }
    "prefs" => effects.push(Effect::OpenPrefs),
    "about" => effects.push(Effect::OpenDialog(DialogSpec::About)),
    "quit" => effects.push(Effect::OpenDialog(DialogSpec::Quit)),
    "otp" => {
      let (tx, _rx) = tokio::sync::oneshot::channel();
      effects.push(Effect::OpenDialog(DialogSpec::Otp(
        "Enter the code from your authenticator app".into(),
        tx,
      )));
    }
    "form" => {
      with_app(|app| app.main.set_portal("vpn.example.com"));
    }
    _ => {}
  }
  with_app(|app| {
    app.status = view.clone();
    app.main.set_status(view);
    if demo == "toast" {
      app
        .main
        .toast("The VPN service refused the connection: gateway is busy".into());
    }
  });
  effects
}

fn capture_png(hwnd: HWND, path: &str) -> anyhow::Result<()> {
  unsafe {
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc)?;
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    let info = BITMAPINFO {
      bmiHeader: BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
      },
      ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let old = SelectObject(dc, bitmap.into());
    // PW_CLIENTONLY | PW_RENDERFULLCONTENT
    let _ = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(3));
    let data = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
    let rgba: Vec<u8> = data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    std::fs::write(path, gfx::encode_png(w as u32, h as u32, &rgba)?)?;
    SelectObject(dc, old);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
  }
  Ok(())
}

// ---- entry point ----------------------------------------------------------------------------------

fn allow_dark_menus() {
  // uxtheme!SetPreferredAppMode (ordinal 135, undocumented): menus follow the app theme.
  unsafe {
    if let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll"))
      && let Some(set_mode) = GetProcAddress(uxtheme, PCSTR(135 as *const u8))
    {
      let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
      set_mode(1); // AllowDark
    }
  }
}

pub fn main() -> ExitCode {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let args: Vec<String> = std::env::args().collect();
  if args.iter().any(|a| a == "--version") {
    println!("gpgui-win {}", env!("CARGO_PKG_VERSION"));
    return ExitCode::SUCCESS;
  }
  unsafe {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
  }
  if let Some(i) = args.iter().position(|a| a == "--export-icons") {
    let dir = std::path::PathBuf::from(args.get(i + 1).map(String::as_str).unwrap_or("."));
    return match gfx::write_ico(Svg::App, &dir.join("app.ico")) {
      Ok(()) => ExitCode::SUCCESS,
      Err(err) => {
        eprintln!("{err:#}");
        ExitCode::FAILURE
      }
    };
  }
  let screenshot = std::env::var("GPGUI_WIN_SCREENSHOT").ok();

  // Single instance: activate the running one instead.
  if screenshot.is_none() {
    let _mutex = unsafe { CreateMutexW(None, true, w!("Local\\GlobalProtectVPN.Instance")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
      if let Ok(hwnd) = unsafe { FindWindowW(CLASS_MAIN, PCWSTR::null()) } {
        unsafe {
          let _ = PostMessageW(Some(hwnd), WM_APP_ACTIVATE, WPARAM(0), LPARAM(0));
        }
      }
      return ExitCode::SUCCESS;
    }
    std::mem::forget(_mutex);
  }

  match run(screenshot, args.iter().any(|a| a == "--minimized")) {
    Ok(code) => code,
    Err(err) => {
      log::error!("{err:#}");
      unsafe {
        MessageBoxW(
          None,
          &HSTRING::from(format!("{err:#}")),
          w!("GlobalProtect VPN"),
          MB_ICONERROR,
        );
      }
      ExitCode::FAILURE
    }
  }
}

fn run(screenshot: Option<String>, minimized: bool) -> anyhow::Result<ExitCode> {
  allow_dark_menus();
  register_classes()?;
  let theme = Theme::system();
  let settings = if screenshot.is_some() {
    Settings::default()
  } else {
    Settings::load()
  };
  let saved_password = (screenshot.is_none() && settings.profile.remember_password)
    .then(|| {
      gpdesktop::secrets::load_password(&settings.profile.portal, &settings.profile.username)
        .ok()
        .flatten()
    })
    .flatten()
    .map(|p| p.to_string());

  let hwnd = create_window(
    CLASS_MAIN,
    "GlobalProtect VPN",
    None,
    main_window::WIDTH,
    main_window::HEIGHT,
    false,
  )
  .ok_or_else(|| anyhow::anyhow!("Cannot create the main window"))?;
  let main = MainWindow::new(hwnd, theme, settings.clone(), saved_password);
  let status = main.view.clone();

  let rt = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2)
    .enable_all()
    .build()?;
  let queue: Arc<Mutex<VecDeque<UiEvent>>> = Arc::default();
  let sink = queue.clone();
  let target = hwnd.0 as isize;
  let controller = controller::spawn(rt.handle(), settings.clone(), move |event| {
    sink.lock().unwrap().push_back(event);
    unsafe {
      let _ = PostMessageW(Some(HWND(target as *mut _)), WM_APP_UI, WPARAM(0), LPARAM(0));
    }
  });

  let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
  let mut tray = tray::Tray::new(hwnd, WM_APP_TRAY);
  if screenshot.is_none() {
    tray.update(&status);
  }
  APP.with(|cell| {
    *cell.borrow_mut() = Some(App {
      _rt: rt,
      controller,
      queue,
      theme,
      main,
      log: None,
      dialogs: Vec::new(),
      log_buf: LogBuffer::default(),
      tray,
      settings: settings.clone(),
      status,
      taskbar_created,
      demo: screenshot.is_some(),
      main_deactivated: None,
    })
  });
  unsafe {
    SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
  }

  if let Some(path) = screenshot {
    let demo = std::env::var("GPGUI_WIN_DEMO").unwrap_or_default();
    show(hwnd);
    let effects = apply_demo(&demo);
    run_effects(effects);
    if demo == "prefs-app" {
      run_effect(Effect::OpenPrefs);
      with_app(|app| {
        app.main.prefs.show_application_tab();
        app.main.layout();
      });
    }
    let deadline = Instant::now() + Duration::from_millis(700);
    pump_until(deadline);
    let shot = with_app(|app| match demo.as_str() {
      "log" => app.log.as_ref().map(|l| l.s.hwnd),
      "about" | "quit" | "otp" => app.dialogs.last().map(|d| d.s.hwnd),
      _ => Some(app.main.s.hwnd),
    })
    .flatten()
    .unwrap_or(hwnd);
    capture_png(shot, &path)?;
    return Ok(ExitCode::SUCCESS);
  }

  autostart::refresh();
  if !(minimized || settings.start_minimized) {
    place_near_tray(hwnd);
    show(hwnd);
  }

  // Message loop. IsDialogMessage gives Tab navigation and turns Enter/Esc into IDOK/IDCANCEL.
  let mut msg = MSG::default();
  unsafe {
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
      if msg.message == WM_KEYDOWN && msg.wParam.0 == 'L' as usize && GetKeyState(VK_CONTROL.0 as i32) < 0 {
        run_effect(Effect::OpenLog);
        continue;
      }
      let top = GetAncestor(msg.hwnd, GA_ROOT);
      if !top.is_invalid() && IsDialogMessageW(top, &msg).as_bool() {
        continue;
      }
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
  }
  APP.with(|cell| cell.borrow_mut().take());
  Ok(ExitCode::SUCCESS)
}

fn pump_until(deadline: Instant) {
  let mut msg = MSG::default();
  while Instant::now() < deadline {
    unsafe {
      while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
      }
    }
    std::thread::sleep(Duration::from_millis(15));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn windows_are_kept_inside_the_work_area() {
    let work = RECT {
      left: 0,
      top: 0,
      right: 2560,
      bottom: 1392,
    };
    // Centred on a main window in the bottom-right corner: pulled back on screen.
    assert_eq!(clamp_to_work(2082, 774, 536, 600, &work, 12), (2012, 774));
    assert_eq!(clamp_to_work(2082, 1000, 536, 600, &work, 12), (2012, 780));
    // Already inside: unchanged.
    assert_eq!(clamp_to_work(800, 300, 536, 600, &work, 12), (800, 300));
    // Off the top-left: pushed in.
    assert_eq!(clamp_to_work(-50, -20, 536, 600, &work, 12), (12, 12));
    // Larger than the work area: aligned to its top-left.
    assert_eq!(clamp_to_work(100, 100, 3000, 2000, &work, 12), (0, 0));
  }
}
