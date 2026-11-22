use egui::{
  pos2, Color32, ColorImage, Context, Painter, Rect, TextureHandle, TextureOptions, Vec2,
};
use winit::window::Icon;

const WINDOW_ICON_SIZE: u32 = 32;
const TASKBAR_ICON_SIZE: u32 = 64;
const UI_LOGO_TEX_SIZE: usize = 256;

// Enger transparenter UV-Ausschnitt des freigestellten Cyber-Ambosses fuer verzerrungsfreie 1:1-Skalierung
const UV_MIN_X: f32 = 0.02_f32;
const UV_MAX_X: f32 = 0.98_f32;
const UV_MIN_Y: f32 = 0.13_f32;
const UV_MAX_Y: f32 = 0.87_f32;

// Direkt zur Compile-Zeit durch denselben 4x4-MSAA-Cyber-Amboss-Renderer in `build.rs` erzeugt (0.0 ms Laufzeit-Overhead)
const CYBER_ANVIL_32_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cyber_anvil_32.rgba"));
const CYBER_ANVIL_64_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cyber_anvil_64.rgba"));
const CYBER_ANVIL_256_RGBA: &[u8] =
  include_bytes!(concat!(env!("OUT_DIR"), "/cyber_anvil_256.rgba"));

#[inline]
fn render_anvil_rgba(size: u32) -> Vec<u8> {
  match size {
    32 => CYBER_ANVIL_32_RGBA.to_vec(),
    64 => CYBER_ANVIL_64_RGBA.to_vec(),
    256 => CYBER_ANVIL_256_RGBA.to_vec(),
    other => resize_bilinear_rgba(CYBER_ANVIL_256_RGBA, 256, other.max(1)),
  }
}

fn resize_bilinear_rgba(src: &[u8], src_size: u32, dst_size: u32) -> Vec<u8> {
  let mut dst = vec![0_u8; (dst_size * dst_size * 4) as usize];
  let scale = (src_size as f32) / (dst_size.max(1) as f32);
  let max_s = (src_size.saturating_sub(1)) as i32;

  for dy in 0..dst_size {
    let sy = ((dy as f32) + 0.5_f32) * scale - 0.5_f32;
    let y0 = (sy.floor() as i32).clamp(0, max_s);
    let y1 = (y0 + 1).clamp(0, max_s);
    let fy = (sy - sy.floor()).clamp(0.0_f32, 1.0_f32);

    for dx in 0..dst_size {
      let sx = ((dx as f32) + 0.5_f32) * scale - 0.5_f32;
      let x0 = (sx.floor() as i32).clamp(0, max_s);
      let x1 = (x0 + 1).clamp(0, max_s);
      let fx = (sx - sx.floor()).clamp(0.0_f32, 1.0_f32);

      let i00 = (((y0 as u32) * src_size + (x0 as u32)) * 4) as usize;
      let i10 = (((y0 as u32) * src_size + (x1 as u32)) * 4) as usize;
      let i01 = (((y1 as u32) * src_size + (x0 as u32)) * 4) as usize;
      let i11 = (((y1 as u32) * src_size + (x1 as u32)) * 4) as usize;

      let w00 = (1.0_f32 - fx) * (1.0_f32 - fy);
      let w10 = fx * (1.0_f32 - fy);
      let w01 = (1.0_f32 - fx) * fy;
      let w11 = fx * fy;

      let out_idx = ((dy * dst_size + dx) * 4) as usize;
      let mut pa = 0.0_f32;
      let mut pr = 0.0_f32;
      let mut pg = 0.0_f32;
      let mut pb = 0.0_f32;
      for &(idx, w) in &[(i00, w00), (i10, w10), (i01, w01), (i11, w11)] {
        let a = (src[idx + 3] as f32) / 255.0_f32;
        pa += a * w;
        pr += (src[idx] as f32) * a * w;
        pg += (src[idx + 1] as f32) * a * w;
        pb += (src[idx + 2] as f32) * a * w;
      }

      let alpha_u8 = (pa.clamp(0.0_f32, 1.0_f32) * 255.0_f32).round() as u8;
      if alpha_u8 > 0 && pa > 1.0e-5_f32 {
        let inv_a = 1.0_f32 / pa;
        dst[out_idx] = (pr * inv_a).clamp(0.0_f32, 255.0_f32).round() as u8;
        dst[out_idx + 1] = (pg * inv_a).clamp(0.0_f32, 255.0_f32).round() as u8;
        dst[out_idx + 2] = (pb * inv_a).clamp(0.0_f32, 255.0_f32).round() as u8;
        dst[out_idx + 3] = alpha_u8;
      }
    }
  }
  dst
}

/// Erzeugt das freigestellte 32x32 4x4-supersampled RGBA-Fenster-Icon (`ICON_SMALL` in der Titelleiste).
pub fn create_window_icon() -> Option<Icon> {
  Icon::from_rgba(render_anvil_rgba(WINDOW_ICON_SIZE), WINDOW_ICON_SIZE, WINDOW_ICON_SIZE).ok()
}

/// Erzeugt das freigestellte 64x64 4x4-supersampled RGBA-Taskleisten-Icon (`ICON_BIG` fuer Windows-Taskleiste & Alt+Tab).
pub fn create_taskbar_icon() -> Option<Icon> {
  Icon::from_rgba(
    render_anvil_rgba(TASKBAR_ICON_SIZE),
    TASKBAR_ICON_SIZE,
    TASKBAR_ICON_SIZE,
  )
  .ok()
}

#[cfg(target_os = "windows")]
mod win32_icon {
  #![allow(dead_code)]

  use core::ffi::c_void;
  use std::os::windows::ffi::OsStrExt;
  use std::sync::OnceLock;

  const WM_SETICON: u32 = 0x0080;
  const ICON_SMALL: usize = 0;
  const ICON_BIG: usize = 1;
  const GCLP_HICON: i32 = -14;
  const GCLP_HICONSM: i32 = -34;

  const DIB_RGB_COLORS: u32 = 0;
  const BI_RGB: u32 = 0;

  const SHCNE_UPDATEITEM: i32 = 0x0000_2000;
  const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
  const SHCNF_IDLIST: u32 = 0x0000;
  const SHCNF_PATHW: u32 = 0x0005;

  // Dauerhaft im Prozess gehaltene Win32-HICON-Handles (64x64 ICON_BIG, 32x32 ICON_SMALL),
  // damit Windows 11 Taskleiste, Alt+Tab und Titelleiste waehrend der gesamten Laufzeit gueltige Handles besitzen.
  static WIN32_PROCESS_HICONS: OnceLock<(isize, isize)> = OnceLock::new();

  #[repr(C)]
  struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
  }

  #[repr(C)]
  struct IconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: *mut c_void,
    hbm_color: *mut c_void,
  }

  #[link(name = "gdi32")]
  extern "system" {
    fn CreateDIBSection(
      hdc: *mut c_void,
      pbmi: *const BitmapInfoHeader,
      usage: u32,
      ppv_bits: *mut *mut c_void,
      h_section: *mut c_void,
      offset: u32,
    ) -> *mut c_void;
    fn CreateBitmap(
      n_width: i32,
      n_height: i32,
      n_planes: u32,
      n_bit_count: u32,
      lp_bits: *const c_void,
    ) -> *mut c_void;
    fn GdiFlush() -> i32;
    fn DeleteObject(ho: *mut c_void) -> i32;
  }

  #[link(name = "user32")]
  extern "system" {
    fn CreateIconIndirect(piconinfo: *const IconInfo) -> *mut c_void;
    fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    #[cfg(target_pointer_width = "64")]
    fn SetClassLongPtrW(hwnd: isize, n_index: i32, dw_new_long: isize) -> usize;
    #[cfg(target_pointer_width = "32")]
    fn SetClassLongW(hwnd: isize, n_index: i32, dw_new_long: i32) -> u32;
  }

  #[link(name = "shell32")]
  extern "system" {
    fn SHChangeNotify(
      w_event_id: i32,
      u_flags: u32,
      dw_item1: *const c_void,
      dw_item2: *const c_void,
    );
  }

  unsafe fn create_argb_win32_hicon(size: u32, rgba: &[u8]) -> isize {
    if size == 0 || rgba.len() < (size * size * 4) as usize {
      return 0;
    }

    let bmi = BitmapInfoHeader {
      bi_size: core::mem::size_of::<BitmapInfoHeader>() as u32,
      bi_width: size as i32,
      bi_height: -(size as i32), // Top-Down 32-Bit PARGB DIB fuer DWM / Taskleiste
      bi_planes: 1,
      bi_bit_count: 32,
      bi_compression: BI_RGB,
      bi_size_image: size * size * 4,
      bi_x_pels_per_meter: 0,
      bi_y_pels_per_meter: 0,
      bi_clr_used: 0,
      bi_clr_important: 0,
    };

    let mut dib_bits: *mut c_void = core::ptr::null_mut();
    let hbm_color = CreateDIBSection(
      core::ptr::null_mut(),
      &bmi,
      DIB_RGB_COLORS,
      &mut dib_bits,
      core::ptr::null_mut(),
      0,
    );
    if hbm_color.is_null() || dib_bits.is_null() {
      if !hbm_color.is_null() {
        let _ = DeleteObject(hbm_color);
      }
      return 0;
    }

    // 32-Bit Premultiplied-BGRA fuer GDI AlphaBlend / DWM-Komposition ohne helle Saumkanten
    let dst = core::slice::from_raw_parts_mut(dib_bits as *mut u8, (size * size * 4) as usize);
    let mask_row_bytes = (((size as usize) + 15) / 16) * 2; // WORD-Alignment fuer CreateBitmap
    let mut mask_bits = vec![0_u8; mask_row_bytes * (size as usize)];

    for y in 0..size {
      let row_mask_off = (y as usize) * mask_row_bytes;
      for x in 0..size {
        let px = ((y * size + x) * 4) as usize;
        let a = rgba[px + 3];
        if a == 0 {
          dst[px] = 0;
          dst[px + 1] = 0;
          dst[px + 2] = 0;
          dst[px + 3] = 0;
          mask_bits[row_mask_off + ((x as usize) / 8)] |= 1_u8 << (7 - (x % 8));
        } else {
          let a16 = a as u16;
          dst[px] = (((rgba[px + 2] as u16) * a16 + 127) / 255) as u8;
          dst[px + 1] = (((rgba[px + 1] as u16) * a16 + 127) / 255) as u8;
          dst[px + 2] = (((rgba[px] as u16) * a16 + 127) / 255) as u8;
          dst[px + 3] = a;
        }
      }
    }
    let _ = GdiFlush();

    let hbm_mask = CreateBitmap(
      size as i32,
      size as i32,
      1,
      1,
      mask_bits.as_ptr() as *const c_void,
    );
    if hbm_mask.is_null() {
      let _ = DeleteObject(hbm_color);
      return 0;
    }

    let icon_info = IconInfo {
      f_icon: 1,
      x_hotspot: 0,
      y_hotspot: 0,
      hbm_mask,
      hbm_color,
    };
    let hicon = CreateIconIndirect(&icon_info);

    // CreateIconIndirect kopiert Color- und Mask-Bitmaps intern; temporaere HBITMAPs sofort freigeben
    let _ = DeleteObject(hbm_mask);
    let _ = DeleteObject(hbm_color);

    hicon as isize
  }

  unsafe fn set_window_class_icon(hwnd: isize, index: i32, hicon: isize) {
    #[cfg(target_pointer_width = "64")]
    {
      let _ = SetClassLongPtrW(hwnd, index, hicon);
    }
    #[cfg(target_pointer_width = "32")]
    {
      let _ = SetClassLongW(hwnd, index, hicon as i32);
    }
  }

  pub(super) fn apply_icons(hwnd_raw: isize) {
    if hwnd_raw == 0 {
      return;
    }

    let &(hicon_64, hicon_32) = WIN32_PROCESS_HICONS.get_or_init(|| {
      let rgba_64 = super::render_anvil_rgba(64);
      let rgba_32 = super::render_anvil_rgba(32);
      unsafe {
        (
          create_argb_win32_hicon(64, &rgba_64),
          create_argb_win32_hicon(32, &rgba_32),
        )
      }
    });

    unsafe {
      if hicon_64 != 0 {
        let _ = SendMessageW(hwnd_raw, WM_SETICON, ICON_BIG, hicon_64);
        set_window_class_icon(hwnd_raw, GCLP_HICON, hicon_64);
      }
      if hicon_32 != 0 {
        let _ = SendMessageW(hwnd_raw, WM_SETICON, ICON_SMALL, hicon_32);
        set_window_class_icon(hwnd_raw, GCLP_HICONSM, hicon_32);
      }

      if let Ok(exe_path) = std::env::current_exe() {
        let exe_wide: Vec<u16> = exe_path
          .as_os_str()
          .encode_wide()
          .chain(core::iter::once(0_u16))
          .collect();
        SHChangeNotify(
          SHCNE_UPDATEITEM,
          SHCNF_PATHW,
          exe_wide.as_ptr() as *const c_void,
          core::ptr::null(),
        );
      }
    }
  }

  pub(super) fn refresh_icon_cache() {
    unsafe {
      SHChangeNotify(
        SHCNE_ASSOCCHANGED,
        SHCNF_IDLIST,
        core::ptr::null(),
        core::ptr::null(),
      );
    }
  }
}

/// Erzeugt echte 32-Bit-PARGB Win32-`HICON`s (`64x64` & `32x32`), setzt sowohl `WM_SETICON` als auch
/// die Window-Class-Icons (`GCLP_HICON` / `GCLP_HICONSM`) fuer die Windows-11-Taskleiste und
/// aktualisiert gezielt den Explorer-Icon-Cache-Eintrag der laufenden `.exe`.
#[allow(dead_code)]
pub fn apply_win32_window_icons(hwnd_raw: isize) {
  #[cfg(target_os = "windows")]
  {
    win32_icon::apply_icons(hwnd_raw);
  }
  #[cfg(not(target_os = "windows"))]
  {
    let _ = hwnd_raw;
  }
}

/// Erzwingt ein Neuladen des Windows-Shell-/Taskleisten-Icon-Caches (`SHCNE_ASSOCCHANGED`).
#[allow(dead_code)]
pub fn force_refresh_windows_icon_cache() {
  #[cfg(target_os = "windows")]
  {
    win32_icon::refresh_icon_cache();
  }
}

/// Zeichnet den freigestellten "Cyber-Amboss FF" als 256x256 RGBA-Textur seitenverhaeltnistreu in `rect`.
pub fn draw_brand_logo(
  ctx: &Context,
  painter: &Painter,
  rect: Rect,
  texture_slot: &mut Option<TextureHandle>,
) {
  let handle = texture_slot.get_or_insert_with(|| {
    let img = ColorImage::from_rgba_unmultiplied(
      [UI_LOGO_TEX_SIZE, UI_LOGO_TEX_SIZE],
      CYBER_ANVIL_256_RGBA,
    );
    ctx.load_texture("ff_cyber_anvil", img, TextureOptions::LINEAR)
  });

  let aspect = (UV_MAX_X - UV_MIN_X) / (UV_MAX_Y - UV_MIN_Y);
  let (avail_w, avail_h) = (rect.width().max(8.0_f32), rect.height().max(8.0_f32));
  let (draw_w, draw_h) = if avail_w / avail_h > aspect {
    (avail_h * aspect, avail_h)
  } else {
    (avail_w, avail_w / aspect)
  };
  let target_rect = Rect::from_center_size(rect.center(), Vec2::new(draw_w, draw_h));
  let uv_rect = Rect::from_min_max(pos2(UV_MIN_X, UV_MIN_Y), pos2(UV_MAX_X, UV_MAX_Y));
  painter.image(handle.id(), target_rect, uv_rect, Color32::WHITE);
}
