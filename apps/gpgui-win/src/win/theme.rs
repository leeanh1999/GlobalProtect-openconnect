//! Light/dark palettes following the Windows app theme.

use windows::{
  Win32::{
    Foundation::COLORREF,
    Graphics::Direct2D::Common::D2D1_COLOR_F,
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
  },
  core::w,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
  pub const fn hex(v: u32) -> Self {
    Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
  }

  pub fn d2d(self) -> D2D1_COLOR_F {
    self.alpha(1.0)
  }

  pub fn alpha(self, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
      r: f32::from(self.0) / 255.0,
      g: f32::from(self.1) / 255.0,
      b: f32::from(self.2) / 255.0,
      a,
    }
  }

  pub fn colorref(self) -> COLORREF {
    COLORREF(u32::from(self.0) | (u32::from(self.1) << 8) | (u32::from(self.2) << 16))
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
  pub dark: bool,
  /// Window background.
  pub bg: Rgb,
  /// Cards and input fields.
  pub surface: Rgb,
  pub surface_hover: Rgb,
  pub border: Rgb,
  pub border_strong: Rgb,
  pub text: Rgb,
  pub text_muted: Rgb,
  pub accent: Rgb,
  pub accent_hover: Rgb,
  pub accent_pressed: Rgb,
  pub on_accent: Rgb,
  pub secondary: Rgb,
  pub secondary_hover: Rgb,
  pub secondary_pressed: Rgb,
  pub success: Rgb,
  pub success_soft: Rgb,
  pub warning: Rgb,
  pub warning_soft: Rgb,
  pub danger: Rgb,
  pub danger_soft: Rgb,
  pub neutral_soft: Rgb,
  pub toast_bg: Rgb,
  pub toast_text: Rgb,
}

impl Theme {
  pub const LIGHT: Theme = Theme {
    dark: false,
    bg: Rgb::hex(0xF3F4F6),
    surface: Rgb::hex(0xFFFFFF),
    surface_hover: Rgb::hex(0xF7F8FA),
    border: Rgb::hex(0xE1E3E8),
    border_strong: Rgb::hex(0xC9CCD3),
    text: Rgb::hex(0x16181D),
    text_muted: Rgb::hex(0x60646C),
    accent: Rgb::hex(0x1A6FDB),
    accent_hover: Rgb::hex(0x1863C4),
    accent_pressed: Rgb::hex(0x1556AB),
    on_accent: Rgb::hex(0xFFFFFF),
    secondary: Rgb::hex(0xE6E8EC),
    secondary_hover: Rgb::hex(0xDCDFE4),
    secondary_pressed: Rgb::hex(0xD0D3DA),
    success: Rgb::hex(0x1E8E50),
    success_soft: Rgb::hex(0xDCF1E4),
    warning: Rgb::hex(0xB7791F),
    warning_soft: Rgb::hex(0xFBF0D6),
    danger: Rgb::hex(0xC42B1C),
    danger_soft: Rgb::hex(0xFBE3E0),
    neutral_soft: Rgb::hex(0xE6E8EC),
    toast_bg: Rgb::hex(0x26292F),
    toast_text: Rgb::hex(0xFFFFFF),
  };

  pub const DARK: Theme = Theme {
    dark: true,
    bg: Rgb::hex(0x1C1D21),
    surface: Rgb::hex(0x27292E),
    surface_hover: Rgb::hex(0x2E3036),
    border: Rgb::hex(0x383B42),
    border_strong: Rgb::hex(0x4A4E57),
    text: Rgb::hex(0xF2F3F5),
    text_muted: Rgb::hex(0xA4A8B0),
    accent: Rgb::hex(0x3B8EF0),
    accent_hover: Rgb::hex(0x4C9AF5),
    accent_pressed: Rgb::hex(0x2F7AD6),
    on_accent: Rgb::hex(0xFFFFFF),
    secondary: Rgb::hex(0x34373E),
    secondary_hover: Rgb::hex(0x3C3F47),
    secondary_pressed: Rgb::hex(0x444851),
    success: Rgb::hex(0x4CC27E),
    success_soft: Rgb::hex(0x1F3A2B),
    warning: Rgb::hex(0xE5B54A),
    warning_soft: Rgb::hex(0x3F3420),
    danger: Rgb::hex(0xFF7B6B),
    danger_soft: Rgb::hex(0x452622),
    neutral_soft: Rgb::hex(0x34373E),
    toast_bg: Rgb::hex(0xF2F3F5),
    toast_text: Rgb::hex(0x16181D),
  };

  /// The current Windows app theme (Settings › Personalization › Colors).
  pub fn system() -> Theme {
    if let Ok(forced) = std::env::var("GPGUI_WIN_THEME") {
      return if forced.eq_ignore_ascii_case("dark") {
        Theme::DARK
      } else {
        Theme::LIGHT
      };
    }
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
      RegGetValueW(
        HKEY_CURRENT_USER,
        w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        w!("AppsUseLightTheme"),
        RRF_RT_REG_DWORD,
        None,
        Some(&mut value as *mut u32 as *mut _),
        Some(&mut size),
      )
    };
    if status.is_ok() && value == 0 {
      Theme::DARK
    } else {
      Theme::LIGHT
    }
  }
}
