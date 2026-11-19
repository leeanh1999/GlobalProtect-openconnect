//! Direct2D / DirectWrite / WIC helpers: factories, drawing primitives,
//! SVG rasterisation (status shields, tray and window icons) and PNG output.

use std::{cell::RefCell, collections::HashMap};

use anyhow::Context;
use windows::{
  Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
      Direct2D::{
        Common::{D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
        D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
        D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
        D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1CreateFactory,
        ID2D1Bitmap, ID2D1DeviceContext5, ID2D1Factory1, ID2D1HwndRenderTarget, ID2D1RenderTarget,
        ID2D1SolidColorBrush,
      },
      DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT,
        DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
        DWRITE_TEXT_METRICS, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
        DWRITE_WORD_WRAPPING_WRAP, DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
      },
      Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
      Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
      },
      Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory, WICBitmapCacheOnLoad,
        WICBitmapLockRead, WICRect,
      },
    },
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    UI::{
      Shell::SHCreateMemStream,
      WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO},
    },
  },
  core::{Interface, PCWSTR, w},
};
use windows_numerics::{Matrix3x2, Vector2};

use super::theme::Rgb;

pub fn rect(left: f32, top: f32, width: f32, height: f32) -> D2D_RECT_F {
  D2D_RECT_F {
    left,
    top,
    right: left + width,
    bottom: top + height,
  }
}

pub fn contains(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
  x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub fn inset(r: &D2D_RECT_F, dx: f32, dy: f32) -> D2D_RECT_F {
  D2D_RECT_F {
    left: r.left + dx,
    top: r.top + dy,
    right: r.right - dx,
    bottom: r.bottom - dy,
  }
}

pub fn wide(s: &str) -> Vec<u16> {
  s.encode_utf16().collect()
}

struct Factories {
  d2d: ID2D1Factory1,
  dwrite: IDWriteFactory,
  wic: IWICImagingFactory,
}

thread_local! {
  static FACTORIES: RefCell<Option<Factories>> = const { RefCell::new(None) };
  static FORMATS: RefCell<HashMap<(u32, u32, u8), IDWriteTextFormat>> = RefCell::new(HashMap::new());
  static SVG_CACHE: RefCell<HashMap<(Svg, u32), Vec<u8>>> = RefCell::new(HashMap::new());
}

fn factories<T>(f: impl FnOnce(&Factories) -> T) -> T {
  FACTORIES.with(|cell| {
    let mut slot = cell.borrow_mut();
    if slot.is_none() {
      let options = D2D1_FACTORY_OPTIONS::default();
      let d2d: ID2D1Factory1 =
        unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options)) }.expect("Direct2D factory");
      let dwrite: IDWriteFactory =
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.expect("DirectWrite factory");
      let wic: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }.expect("WIC factory");
      *slot = Some(Factories { d2d, dwrite, wic });
    }
    f(slot.as_ref().unwrap())
  })
}

// ---- text -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
  Leading,
  Center,
  Trailing,
}

#[derive(Debug, Clone, Copy)]
pub struct Font {
  pub size: f32,
  pub semibold: bool,
  pub mono: bool,
}

impl Font {
  pub const fn body(size: f32) -> Self {
    Font {
      size,
      semibold: false,
      mono: false,
    }
  }
  pub const fn strong(size: f32) -> Self {
    Font {
      size,
      semibold: true,
      mono: false,
    }
  }
}

fn font_family(mono: bool) -> PCWSTR {
  if mono {
    w!("Cascadia Mono")
  } else {
    w!("Segoe UI Variable Text")
  }
}

fn text_format(font: Font) -> IDWriteTextFormat {
  let key = (font.size.to_bits(), u32::from(font.semibold), u8::from(font.mono));
  if let Some(format) = FORMATS.with(|f| f.borrow().get(&key).cloned()) {
    return format;
  }
  let weight: DWRITE_FONT_WEIGHT = if font.semibold {
    DWRITE_FONT_WEIGHT_SEMI_BOLD
  } else {
    DWRITE_FONT_WEIGHT_NORMAL
  };
  let format = factories(|f| unsafe {
    f.dwrite
      .CreateTextFormat(
        font_family(font.mono),
        None,
        weight,
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        font.size,
        w!("en-us"),
      )
      .or_else(|_| {
        // Older Windows 10 builds lack Segoe UI Variable / Cascadia.
        f.dwrite.CreateTextFormat(
          if font.mono { w!("Consolas") } else { w!("Segoe UI") },
          None,
          weight,
          DWRITE_FONT_STYLE_NORMAL,
          DWRITE_FONT_STRETCH_NORMAL,
          font.size,
          w!("en-us"),
        )
      })
      .expect("text format")
  });
  FORMATS.with(|f| f.borrow_mut().insert(key, format.clone()));
  format
}

/// Height of `text` laid out at `width` (wrapping).
pub fn measure(text: &str, font: Font, width: f32, wrap: bool) -> (f32, f32) {
  let format = text_format(font);
  let text = wide(text);
  factories(|f| unsafe {
    let Ok(layout) = f.dwrite.CreateTextLayout(&text, &format, width.max(1.0), 10_000.0) else {
      return (0.0, 0.0);
    };
    let _ = layout.SetWordWrapping(if wrap {
      DWRITE_WORD_WRAPPING_WRAP
    } else {
      DWRITE_WORD_WRAPPING_NO_WRAP
    });
    let mut metrics = DWRITE_TEXT_METRICS::default();
    let _ = layout.GetMetrics(&mut metrics);
    (metrics.widthIncludingTrailingWhitespace, metrics.height)
  })
}

// ---- drawing context -----------------------------------------------------------

/// A frame being drawn on a window (or an off-screen bitmap).
pub struct Canvas<'a> {
  pub rt: &'a ID2D1RenderTarget,
  brush: ID2D1SolidColorBrush,
}

impl<'a> Canvas<'a> {
  pub fn new(rt: &'a ID2D1RenderTarget) -> anyhow::Result<Self> {
    let brush = unsafe { rt.CreateSolidColorBrush(&Rgb(0, 0, 0).d2d(), None)? };
    Ok(Self { rt, brush })
  }

  fn brush(&self, color: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
    unsafe { self.brush.SetColor(&color) };
    &self.brush
  }

  pub fn clear(&self, color: Rgb) {
    unsafe { self.rt.Clear(Some(&color.d2d())) };
  }

  pub fn fill_round(&self, r: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
    let rr = D2D1_ROUNDED_RECT {
      rect: r,
      radiusX: radius,
      radiusY: radius,
    };
    unsafe { self.rt.FillRoundedRectangle(&rr, self.brush(color)) };
  }

  pub fn stroke_round(&self, r: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F, width: f32) {
    // Keep 1-DIP strokes crisp by centring them on the pixel grid.
    let h = width / 2.0;
    let rr = D2D1_ROUNDED_RECT {
      rect: inset(&r, h, h),
      radiusX: radius,
      radiusY: radius,
    };
    unsafe { self.rt.DrawRoundedRectangle(&rr, self.brush(color), width, None) };
  }

  pub fn fill_circle(&self, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F) {
    let e = D2D1_ELLIPSE {
      point: Vector2 { X: cx, Y: cy },
      radiusX: r,
      radiusY: r,
    };
    unsafe { self.rt.FillEllipse(&e, self.brush(color)) };
  }

  pub fn stroke_circle(&self, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F, width: f32) {
    let e = D2D1_ELLIPSE {
      point: Vector2 { X: cx, Y: cy },
      radiusX: r,
      radiusY: r,
    };
    unsafe { self.rt.DrawEllipse(&e, self.brush(color), width, None) };
  }

  pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: D2D1_COLOR_F, width: f32) {
    unsafe {
      self.rt.DrawLine(
        Vector2 { X: x0, Y: y0 },
        Vector2 { X: x1, Y: y1 },
        self.brush(color),
        width,
        None,
      )
    };
  }

  /// Text in `r`; `wrap` allows multiple lines, otherwise it is trimmed with an ellipsis.
  #[allow(clippy::too_many_arguments)]
  pub fn text(&self, s: &str, r: D2D_RECT_F, font: Font, color: D2D1_COLOR_F, align: Align, vcenter: bool, wrap: bool) {
    let format = text_format(font);
    unsafe {
      let _ = format.SetTextAlignment(match align {
        Align::Leading => DWRITE_TEXT_ALIGNMENT_LEADING,
        Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
        Align::Trailing => DWRITE_TEXT_ALIGNMENT_TRAILING,
      } as DWRITE_TEXT_ALIGNMENT);
      let _ = format.SetParagraphAlignment(if vcenter {
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER
      } else {
        DWRITE_PARAGRAPH_ALIGNMENT_NEAR
      });
      let _ = format.SetWordWrapping(if wrap {
        DWRITE_WORD_WRAPPING_WRAP
      } else {
        DWRITE_WORD_WRAPPING_NO_WRAP
      });
      if !wrap {
        let trimming = DWRITE_TRIMMING {
          granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
          delimiter: 0,
          delimiterCount: 0,
        };
        let sign = factories(|f| f.dwrite.CreateEllipsisTrimmingSign(&format)).ok();
        let _ = format.SetTrimming(&trimming, sign.as_ref());
      } else {
        let none = DWRITE_TRIMMING::default();
        let _ = format.SetTrimming(&none, None);
      }
      let text = wide(s);
      self.rt.DrawText(
        &text,
        &format,
        &r,
        self.brush(color),
        if wrap {
          D2D1_DRAW_TEXT_OPTIONS_NONE
        } else {
          D2D1_DRAW_TEXT_OPTIONS_CLIP
        },
        DWRITE_MEASURING_MODE_NATURAL,
      );
    }
  }

  /// A rasterised SVG scaled into `r` (rendered at the target's pixel density).
  pub fn svg(&self, svg: Svg, r: D2D_RECT_F, dpi_scale: f32) {
    let px = ((r.right - r.left) * dpi_scale).round().max(1.0) as u32;
    let Ok(bitmap) = svg_bitmap(self.rt, svg, px) else {
      return;
    };
    unsafe {
      self.rt.DrawBitmap(
        &bitmap,
        Some(&r),
        1.0,
        windows::Win32::Graphics::Direct2D::D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
        None,
      )
    };
  }
}

// ---- window render target ---------------------------------------------------------

pub struct WindowTarget {
  rt: Option<ID2D1HwndRenderTarget>,
}

impl WindowTarget {
  pub const fn new() -> Self {
    Self { rt: None }
  }

  pub fn discard(&mut self) {
    self.rt = None;
  }

  pub fn resize(&mut self, width: u32, height: u32) {
    if let Some(rt) = &self.rt {
      unsafe {
        let _ = rt.Resize(&D2D_SIZE_U { width, height });
      }
    }
  }

  /// Draws a frame with `draw` (coordinates in DIPs at `scale` × 96 DPI).
  pub fn draw(&mut self, hwnd: HWND, scale: f32, draw: impl FnOnce(&Canvas)) -> anyhow::Result<()> {
    if self.rt.is_none() {
      let mut rc = RECT::default();
      unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc)? };
      let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
          format: DXGI_FORMAT_B8G8R8A8_UNORM,
          alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 0.0,
        dpiY: 0.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
      };
      let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: D2D_SIZE_U {
          width: (rc.right - rc.left).max(1) as u32,
          height: (rc.bottom - rc.top).max(1) as u32,
        },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
      };
      let rt = factories(|f| unsafe { f.d2d.CreateHwndRenderTarget(&props, &hwnd_props) })?;
      self.rt = Some(rt);
    }
    let rt = self.rt.as_ref().unwrap();
    unsafe {
      rt.SetDpi(96.0 * scale, 96.0 * scale);
      rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
      rt.BeginDraw();
      rt.SetTransform(&Matrix3x2::identity());
    }
    let base: ID2D1RenderTarget = rt.cast()?;
    let result = Canvas::new(&base).map(|canvas| draw(&canvas));
    let end = unsafe { rt.EndDraw(None, None) };
    if end.is_err() {
      // D2DERR_RECREATE_TARGET and friends: rebuild on the next paint.
      self.rt = None;
    }
    result
  }
}

// ---- SVG ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Svg {
  App,
  Connected,
  Connecting,
  Disconnected,
  Error,
}

impl Svg {
  fn source(self) -> &'static str {
    match self {
      Svg::App => include_str!("../../res/icons/app.svg"),
      Svg::Connected => include_str!("../../res/icons/connected.svg"),
      Svg::Connecting => include_str!("../../res/icons/connecting.svg"),
      Svg::Disconnected => include_str!("../../res/icons/disconnected.svg"),
      Svg::Error => include_str!("../../res/icons/error.svg"),
    }
  }

  fn viewbox(self) -> f32 {
    if self == Svg::App { 128.0 } else { 64.0 }
  }
}

/// Renders `svg` into a `px`×`px` premultiplied BGRA buffer (cached).
pub fn svg_pixels(svg: Svg, px: u32) -> anyhow::Result<Vec<u8>> {
  if let Some(pixels) = SVG_CACHE.with(|c| c.borrow().get(&(svg, px)).cloned()) {
    return Ok(pixels);
  }
  let pixels = factories(|f| -> anyhow::Result<Vec<u8>> {
    unsafe {
      let bitmap: IWICBitmap = f
        .wic
        .CreateBitmap(px, px, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
      let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
          format: DXGI_FORMAT_B8G8R8A8_UNORM,
          alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
      };
      let rt = f.d2d.CreateWicBitmapRenderTarget(&bitmap, &props)?;
      let dc: ID2D1DeviceContext5 = rt.cast().context("SVG rendering needs Windows 10 1703 or later")?;
      let stream = SHCreateMemStream(Some(svg.source().as_bytes())).context("SVG stream")?;
      let size = svg.viewbox();
      let doc = dc.CreateSvgDocument(
        &stream,
        D2D_SIZE_F {
          width: size,
          height: size,
        },
      )?;
      let scale = px as f32 / size;
      dc.BeginDraw();
      dc.Clear(Some(&D2D1_COLOR_F::default()));
      dc.SetTransform(&Matrix3x2 {
        M11: scale,
        M12: 0.0,
        M21: 0.0,
        M22: scale,
        M31: 0.0,
        M32: 0.0,
      });
      dc.DrawSvgDocument(&doc);
      dc.EndDraw(None, None)?;

      let lock = bitmap.Lock(
        &WICRect {
          X: 0,
          Y: 0,
          Width: px as i32,
          Height: px as i32,
        },
        WICBitmapLockRead.0 as u32,
      )?;
      let stride = lock.GetStride()? as usize;
      let mut len = 0u32;
      let mut ptr = std::ptr::null_mut();
      lock.GetDataPointer(&mut len, &mut ptr)?;
      let data = std::slice::from_raw_parts(ptr, len as usize);
      let row = px as usize * 4;
      let mut out = Vec::with_capacity(row * px as usize);
      for y in 0..px as usize {
        out.extend_from_slice(&data[y * stride..y * stride + row]);
      }
      Ok(out)
    }
  })?;
  SVG_CACHE.with(|c| c.borrow_mut().insert((svg, px), pixels.clone()));
  Ok(pixels)
}

fn svg_bitmap(rt: &ID2D1RenderTarget, svg: Svg, px: u32) -> anyhow::Result<ID2D1Bitmap> {
  let pixels = svg_pixels(svg, px)?;
  let props = windows::Win32::Graphics::Direct2D::D2D1_BITMAP_PROPERTIES {
    pixelFormat: D2D1_PIXEL_FORMAT {
      format: DXGI_FORMAT_B8G8R8A8_UNORM,
      alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    dpiX: 96.0,
    dpiY: 96.0,
  };
  Ok(unsafe {
    rt.CreateBitmap(
      D2D_SIZE_U { width: px, height: px },
      Some(pixels.as_ptr() as *const _),
      px * 4,
      &props,
    )?
  })
}

/// Straight (non-premultiplied) RGBA, as icons and PNG expect.
pub fn unpremultiply_to_rgba(bgra: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(bgra.len());
  for p in bgra.chunks_exact(4) {
    let a = p[3];
    let un = |c: u8| {
      if a == 0 {
        0
      } else {
        ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
      }
    };
    out.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), a]);
  }
  out
}

/// A `px`×`px` HICON of `svg` (window / tray icons).
pub fn svg_icon(svg: Svg, px: u32) -> anyhow::Result<HICON> {
  let premul = svg_pixels(svg, px)?;
  let rgba = unpremultiply_to_rgba(&premul);
  unsafe {
    let header = BITMAPINFOHEADER {
      biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: px as i32,
      biHeight: -(px as i32),
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    };
    let info = BITMAPINFO {
      bmiHeader: header,
      ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let color: HBITMAP = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (px * px * 4) as usize);
    for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
      d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
    }
    let mask = CreateBitmap(px as i32, px as i32, 1, 1, None);
    let info = ICONINFO {
      fIcon: true.into(),
      xHotspot: 0,
      yHotspot: 0,
      hbmMask: mask,
      hbmColor: color,
    };
    let icon = CreateIconIndirect(&info);
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    Ok(icon?)
  }
}

/// Encodes straight RGBA as PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
  let mut out = Vec::new();
  {
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
  }
  Ok(out)
}

/// Writes a multi-size `.ico` (PNG-compressed entries) of `svg`.
pub fn write_ico(svg: Svg, path: &std::path::Path) -> anyhow::Result<()> {
  const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
  let images: Vec<(u32, Vec<u8>)> = SIZES
    .iter()
    .map(|&px| {
      let rgba = unpremultiply_to_rgba(&svg_pixels(svg, px)?);
      Ok((px, encode_png(px, px, &rgba)?))
    })
    .collect::<anyhow::Result<_>>()?;
  let mut ico = Vec::new();
  ico.extend_from_slice(&[0, 0, 1, 0]);
  ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
  let mut offset = 6 + 16 * images.len() as u32;
  for (px, data) in &images {
    let dim = if *px >= 256 { 0 } else { *px as u8 };
    ico.extend_from_slice(&[dim, dim, 0, 0]);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
    ico.extend_from_slice(&offset.to_le_bytes());
    offset += data.len() as u32;
  }
  for (_, data) in &images {
    ico.extend_from_slice(data);
  }
  std::fs::write(path, ico)?;
  Ok(())
}
