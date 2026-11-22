use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// Alle ICO-Ebenen strikt < 256 (bWidth niemals 0), damit Windows 11 Explorer & Taskleiste
// jedes unkomprimierte 32-Bit-BGRA-DIB ohne PNG-Kompressionszwang sofort akzeptieren.
const ICON_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 128];

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=csrc/ffx_fsr3_bridge.cpp");
  println!("cargo:rerun-if-changed=csrc/xess3_bridge.cpp");
  println!("cargo:rerun-if-changed=csrc/dlss45_bridge.cpp");
  println!("cargo:rerun-if-changed=third_party/FidelityFX-SDK/Kits/FidelityFX/api/include/ffx_api.h");
  println!("cargo:rerun-if-changed=third_party/FidelityFX-SDK/Kits/FidelityFX/upscalers/include/ffx_upscale.h");
  println!("cargo:rerun-if-changed=third_party/xess/inc/xess_d3d12.h");
  println!("cargo:rerun-if-changed=third_party/streamline/include/sl.h");

  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR ist nicht gesetzt"));
  let ico_path = out_dir.join("frameforge.ico");
  let (ico_bytes, rgba_32, rgba_64, rgba_256) = build_multi_res_ico_and_runtime_rgba(&ICON_SIZES);

  fs::write(&ico_path, ico_bytes).expect("Schreiben von frameforge.ico fehlgeschlagen");
  fs::write(out_dir.join("cyber_anvil_32.rgba"), rgba_32)
    .expect("Schreiben von cyber_anvil_32.rgba fehlgeschlagen");
  fs::write(out_dir.join("cyber_anvil_64.rgba"), rgba_64)
    .expect("Schreiben von cyber_anvil_64.rgba fehlgeschlagen");
  fs::write(out_dir.join("cyber_anvil_256.rgba"), rgba_256)
    .expect("Schreiben von cyber_anvil_256.rgba fehlgeschlagen");

  // Fester Icon-Pfad unter <CARGO_MANIFEST_DIR>/target/frameforge.ico fuer den WiX-.msi-Installer
  let manifest_dir =
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR ist nicht gesetzt"));
  let target_dir = manifest_dir.join("target");
  fs::create_dir_all(&target_dir).expect("Erstellen des target-Ordners fehlgeschlagen");
  fs::copy(&ico_path, target_dir.join("frameforge.ico"))
    .expect("Kopieren von frameforge.ico nach target/frameforge.ico fehlgeschlagen");

  if env::var_os("CARGO_CFG_WINDOWS").is_some() {
    // Zwingend Forward-Slashes fuer rc.exe, damit "\f" in "...\out\frameforge.ico"
    // im generierten .rc-Skript niemals als Form-Feed-Escape fehlinterpretiert wird!
    let ico_rc_path = ico_path.to_string_lossy().replace('\\', "/");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico_rc_path);
    res.set("ProductName", "FrameForge");
    res.set("FileDescription", "FrameForge - DirectX 12 Capture & Frame Generation");
    res.set("FileVersion", "1.0.0.0");
    res.set("ProductVersion", "1.0.0.0");
    res.compile().expect("winresource icon compile failed");
  }

  compile_ffx_fsr3_bridge();
  compile_xess3_bridge_if_present();
  compile_dlss45_bridge_if_present();
}

fn compile_ffx_fsr3_bridge() {
  let mut build = cc::Build::new();
  build
    .cpp(true)
    .file("csrc/ffx_fsr3_bridge.cpp")
    .std("c++17")
    .flag_if_supported("/EHsc")
    .flag_if_supported("/utf-8");

  let ffx_api_header = Path::new("third_party/FidelityFX-SDK/Kits/FidelityFX/api/include/ffx_api.h");
  if ffx_api_header.exists() {
    build
      .define("FFX_SDK_AVAILABLE", "1")
      .include("third_party/FidelityFX-SDK/Kits/FidelityFX/api/include")
      .include("third_party/FidelityFX-SDK/Kits/FidelityFX/framegeneration/include");

    let ffx_upscalers_inc = Path::new("third_party/FidelityFX-SDK/Kits/FidelityFX/upscalers/include");
    if ffx_upscalers_inc.exists() {
      build.include(ffx_upscalers_inc);
    }
  }

  build.compile("ffx_fsr3_bridge");
}

fn compile_xess3_bridge_if_present() {
  let xess_src = Path::new("csrc/xess3_bridge.cpp");
  if !xess_src.exists() {
    return;
  }

  let mut build = cc::Build::new();
  build
    .cpp(true)
    .file(xess_src)
    .std("c++17")
    .flag_if_supported("/EHsc")
    .flag_if_supported("/utf-8");

  let xess_inc = Path::new("third_party/xess/inc");
  if xess_inc.exists() && xess_inc.join("xess_d3d12.h").exists() {
    build
      .define("XESS3_SDK_AVAILABLE", "1")
      .include(xess_inc);
  }

  build.compile("xess3_bridge");
}

fn compile_dlss45_bridge_if_present() {
  let dlss_src = Path::new("csrc/dlss45_bridge.cpp");
  if !dlss_src.exists() {
    return;
  }

  let mut build = cc::Build::new();
  build
    .cpp(true)
    .file(dlss_src)
    .std("c++17")
    .flag_if_supported("/EHsc")
    .flag_if_supported("/utf-8");

  let sl_header = Path::new("third_party/streamline/include/sl.h");
  if sl_header.exists() {
    build
      .define("SL_SDK_AVAILABLE", "1")
      .include("third_party/streamline/include");
  }

  build.compile("dlss45_bridge");
}

fn build_multi_res_ico_and_runtime_rgba(sizes: &[u32]) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
  let geom = CyberAnvilGeometry::build();
  let mut dib_blobs: Vec<Vec<u8>> = Vec::with_capacity(sizes.len());
  let mut rgba_32: Option<Vec<u8>> = None;
  let mut rgba_64: Option<Vec<u8>> = None;

  for &sz in sizes {
    let rgba = rasterize_cyber_anvil_rgba(&geom, sz);
    if sz == 32 {
      rgba_32 = Some(rgba.clone());
    } else if sz == 64 {
      rgba_64 = Some(rgba.clone());
    }
    dib_blobs.push(encode_ico_dib_from_rgba(sz, &rgba));
  }

  let rgba_32 = rgba_32.unwrap_or_else(|| rasterize_cyber_anvil_rgba(&geom, 32));
  let rgba_64 = rgba_64.unwrap_or_else(|| rasterize_cyber_anvil_rgba(&geom, 64));
  let rgba_256 = rasterize_cyber_anvil_rgba(&geom, 256);

  let header_bytes = 6_usize + sizes.len() * 16_usize;
  let total_bytes = header_bytes + dib_blobs.iter().map(Vec::len).sum::<usize>();
  let mut ico = Vec::with_capacity(total_bytes);

  // ICONDIR (6 Bytes)
  ico.extend_from_slice(&0_u16.to_le_bytes());
  ico.extend_from_slice(&1_u16.to_le_bytes());
  ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

  // ICONDIRENTRY pro Aufloesung (je 16 Bytes, alle Groessen < 256 -> echtes Byte bWidth/bHeight)
  let mut current_offset = header_bytes as u32;
  for (idx, &size) in sizes.iter().enumerate() {
    let dim_byte = if size >= 256 { 0_u8 } else { size as u8 };
    let blob_len = dib_blobs[idx].len() as u32;
    ico.extend_from_slice(&[dim_byte, dim_byte, 0_u8, 0_u8]);
    ico.extend_from_slice(&1_u16.to_le_bytes());
    ico.extend_from_slice(&32_u16.to_le_bytes());
    ico.extend_from_slice(&blob_len.to_le_bytes());
    ico.extend_from_slice(&current_offset.to_le_bytes());
    current_offset = current_offset.saturating_add(blob_len);
  }

  for blob in dib_blobs {
    ico.extend_from_slice(&blob);
  }

  (ico, rgba_32, rgba_64, rgba_256)
}

fn encode_ico_dib_from_rgba(size: u32, rgba: &[u8]) -> Vec<u8> {
  let xor_bytes = (size * size * 4) as usize;
  let mask_row_bytes = (((size + 31) / 32) * 4) as usize;
  let and_bytes = mask_row_bytes * (size as usize);
  let mut dib = Vec::with_capacity(40 + xor_bytes + and_bytes);

  // BITMAPINFOHEADER (40 Bytes, verdoppelte biHeight fuer XOR + AND innerhalb des ICO-DIB)
  dib.extend_from_slice(&40_u32.to_le_bytes());
  dib.extend_from_slice(&(size as i32).to_le_bytes());
  dib.extend_from_slice(&((size * 2) as i32).to_le_bytes());
  dib.extend_from_slice(&1_u16.to_le_bytes());
  dib.extend_from_slice(&32_u16.to_le_bytes());
  dib.extend_from_slice(&0_u32.to_le_bytes());
  dib.extend_from_slice(&((xor_bytes + and_bytes) as u32).to_le_bytes());
  dib.extend_from_slice(&0_i32.to_le_bytes());
  dib.extend_from_slice(&0_i32.to_le_bytes());
  dib.extend_from_slice(&0_u32.to_le_bytes());
  dib.extend_from_slice(&0_u32.to_le_bytes());

  // 32-Bit Straight-Alpha-BGRA XOR-Maske (Bottom-Up: keine dunklen/weissen Halos an Kanten)
  for y in (0..size).rev() {
    for x in 0..size {
      let idx = ((y * size + x) * 4) as usize;
      dib.extend_from_slice(&[rgba[idx + 2], rgba[idx + 1], rgba[idx], rgba[idx + 3]]);
    }
  }

  // 1-Bit AND-Maske mit 4-Byte-(DWORD)-Zeilen-Alignment (Bottom-Up, Bit = 1 wo alpha == 0)
  let mut row_buf = vec![0_u8; mask_row_bytes];
  for y in (0..size).rev() {
    row_buf.fill(0_u8);
    for x in 0..size {
      if rgba[((y * size + x) * 4 + 3) as usize] == 0 {
        row_buf[(x / 8) as usize] |= 1_u8 << (7 - (x % 8));
      }
    }
    dib.extend_from_slice(&row_buf);
  }
  dib
}

struct CyberAnvilGeometry {
  anvil: Vec<[f32; 2]>,
  f1: [[f32; 2]; 10],
  f2: [[f32; 2]; 10],
}

impl CyberAnvilGeometry {
  fn build() -> Self {
    let mut anvil = Vec::with_capacity(68);
    anvil.push([0.048_f32, 0.298_f32]);
    anvil.push([0.052_f32, 0.290_f32]);
    push_quad_bezier(&mut anvil, [0.052_f32, 0.290_f32], [0.175_f32, 0.274_f32], [0.304_f32, 0.268_f32], 8);
    anvil.push([0.308_f32, 0.244_f32]);
    anvil.push([0.332_f32, 0.244_f32]);
    anvil.push([0.338_f32, 0.205_f32]);
    anvil.push([0.952_f32, 0.205_f32]);
    anvil.push([0.930_f32, 0.284_f32]);
    anvil.push([0.868_f32, 0.308_f32]);
    push_quad_bezier(&mut anvil, [0.868_f32, 0.308_f32], [0.732_f32, 0.392_f32], [0.652_f32, 0.560_f32], 10);
    push_quad_bezier(&mut anvil, [0.652_f32, 0.560_f32], [0.646_f32, 0.668_f32], [0.790_f32, 0.756_f32], 8);
    anvil.push([0.790_f32, 0.830_f32]);
    anvil.push([0.628_f32, 0.830_f32]);
    anvil.push([0.620_f32, 0.794_f32]);
    push_quad_bezier(&mut anvil, [0.620_f32, 0.794_f32], [0.530_f32, 0.662_f32], [0.440_f32, 0.794_f32], 10);
    anvil.push([0.432_f32, 0.830_f32]);
    anvil.push([0.270_f32, 0.830_f32]);
    anvil.push([0.270_f32, 0.756_f32]);
    push_quad_bezier(&mut anvil, [0.270_f32, 0.756_f32], [0.376_f32, 0.676_f32], [0.368_f32, 0.580_f32], 8);
    push_quad_bezier(&mut anvil, [0.368_f32, 0.580_f32], [0.366_f32, 0.524_f32], [0.332_f32, 0.508_f32], 6);
    anvil.push([0.308_f32, 0.508_f32]);
    anvil.push([0.304_f32, 0.494_f32]);
    push_cubic_bezier(&mut anvil, [0.304_f32, 0.494_f32], [0.225_f32, 0.468_f32], [0.125_f32, 0.365_f32], [0.052_f32, 0.306_f32], 10);

    let f1 = [
      [0.484_f32, 0.268_f32], [0.582_f32, 0.268_f32], [0.565_f32, 0.326_f32], [0.517_f32, 0.326_f32], [0.496_f32, 0.394_f32],
      [0.535_f32, 0.394_f32], [0.518_f32, 0.450_f32], [0.479_f32, 0.450_f32], [0.447_f32, 0.548_f32], [0.397_f32, 0.558_f32],
    ];
    let f2 = [
      [0.612_f32, 0.268_f32], [0.768_f32, 0.268_f32], [0.751_f32, 0.326_f32], [0.647_f32, 0.326_f32], [0.626_f32, 0.394_f32],
      [0.688_f32, 0.394_f32], [0.671_f32, 0.450_f32], [0.609_f32, 0.450_f32], [0.574_f32, 0.568_f32], [0.519_f32, 0.578_f32],
    ];
    Self { anvil, f1, f2 }
  }
}

fn push_quad_bezier(out: &mut Vec<[f32; 2]>, p0: [f32; 2], c: [f32; 2], p1: [f32; 2], steps: usize) {
  for step in 1..=steps {
    let t = (step as f32) / (steps as f32);
    let inv = 1.0_f32 - t;
    out.push([
      inv * inv * p0[0] + 2.0_f32 * inv * t * c[0] + t * t * p1[0],
      inv * inv * p0[1] + 2.0_f32 * inv * t * c[1] + t * t * p1[1],
    ]);
  }
}

fn push_cubic_bezier(out: &mut Vec<[f32; 2]>, p0: [f32; 2], c1: [f32; 2], c2: [f32; 2], p1: [f32; 2], steps: usize) {
  for step in 1..=steps {
    let t = (step as f32) / (steps as f32);
    let inv = 1.0_f32 - t;
    let (b0, b1, b2, b3) = (inv * inv * inv, 3.0_f32 * inv * inv * t, 3.0_f32 * inv * t * t, t * t * t);
    out.push([
      b0 * p0[0] + b1 * c1[0] + b2 * c2[0] + b3 * p1[0],
      b0 * p0[1] + b1 * c1[1] + b2 * c2[1] + b3 * p1[1],
    ]);
  }
}

fn rasterize_cyber_anvil_rgba(geom: &CyberAnvilGeometry, size: u32) -> Vec<u8> {
  let inv_size = 1.0_f32 / (size.max(1) as f32);
  let aa_px = inv_size * 0.65_f32;
  let include_details = size >= 32;
  let include_sparks = size >= 64;
  let mut out = vec![0_u8; (size * size * 4) as usize];

  for py in 0..size {
    let v_c = ((py as f32) + 0.5_f32) * inv_size;
    if v_c < 0.13_f32 - inv_size * 2.0_f32 || v_c > 0.87_f32 + inv_size * 2.0_f32 { continue; }
    for px in 0..size {
      let u_c = ((px as f32) + 0.5_f32) * inv_size;
      if u_c < 0.03_f32 - inv_size * 2.0_f32 || u_c > 0.97_f32 + inv_size * 2.0_f32 { continue; }
      if sd_polygon(&geom.anvil, u_c, v_c) > inv_size * 2.5_f32 && (!include_sparks || !near_any_spark(u_c, v_c, inv_size)) {
        continue;
      }

      let (mut pr, mut pg, mut pb, mut pa) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
      for sy in 0..4_u32 {
        let v = ((py as f32) + ((sy as f32) + 0.5_f32) * 0.25_f32) * inv_size;
        for sx in 0..4_u32 {
          let u = ((px as f32) + ((sx as f32) + 0.5_f32) * 0.25_f32) * inv_size;
          let [r, g, b, a] = sample_cyber_anvil(&geom, u, v, aa_px, include_details, include_sparks);
          pr += r * a;
          pg += g * a;
          pb += b * a;
          pa += a;
        }
      }

      let alpha_u8 = ((pa * (1.0_f32 / 16.0_f32)).clamp(0.0_f32, 1.0_f32) * 255.0_f32).round() as u8;
      if alpha_u8 > 0 && pa > 1.0e-5_f32 {
        let idx = ((py * size + px) * 4) as usize;
        let inv_a = 1.0_f32 / pa;
        out[idx] = ((pr * inv_a).clamp(0.0_f32, 1.0_f32) * 255.0_f32).round() as u8;
        out[idx + 1] = ((pg * inv_a).clamp(0.0_f32, 1.0_f32) * 255.0_f32).round() as u8;
        out[idx + 2] = ((pb * inv_a).clamp(0.0_f32, 1.0_f32) * 255.0_f32).round() as u8;
        out[idx + 3] = alpha_u8;
      }
    }
  }
  out
}

const SPARKS: [([f32; 2], f32, bool); 6] = [
  ([0.145_f32, 0.226_f32], 0.0085_f32, true),
  ([0.228_f32, 0.198_f32], 0.0065_f32, true),
  ([0.192_f32, 0.518_f32], 0.0072_f32, true),
  ([0.908_f32, 0.356_f32], 0.0088_f32, false),
  ([0.822_f32, 0.448_f32], 0.0068_f32, false),
  ([0.858_f32, 0.672_f32], 0.0075_f32, false),
];

#[inline]
fn near_any_spark(u: f32, v: f32, inv_size: f32) -> bool {
  let pad = 0.024_f32 + inv_size * 2.0_f32;
  SPARKS.iter().any(|&(pos, _, _)| (u - pos[0]).abs() <= pad && (v - pos[1]).abs() <= pad)
}

fn sample_cyber_anvil(
  geom: &CyberAnvilGeometry,
  u: f32,
  v: f32,
  aa_px: f32,
  include_details: bool,
  include_sparks: bool,
) -> [f32; 4] {
  let d_anvil = sd_polygon(&geom.anvil, u, v);
  let anvil_cov = smooth_cov(d_anvil, aa_px);
  if anvil_cov <= 0.0_f32 {
    return if include_sparks { sample_sparks(u, v, aa_px) } else { [0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32] };
  }

  let groove_delta = u - (0.610_f32 - (v - 0.225_f32) * 0.30_f32);
  let is_left_half = groove_delta < 0.0_f32;

  let mut col = if u < 0.306_f32 {
    let t_h = ((u - 0.048_f32) / 0.256_f32).clamp(0.0_f32, 1.0_f32);
    let y_top = 0.292_f32 - 0.024_f32 * t_h;
    let y_ridge = 0.299_f32 + 0.056_f32 * t_h;
    let y_bot = 0.304_f32 + 0.190_f32 * (0.35_f32 * t_h + 0.65_f32 * t_h * t_h);
    let mut hc = if v <= y_ridge {
      let f = ((v - y_top) / (y_ridge - y_top).max(0.008_f32)).clamp(0.0_f32, 1.0_f32);
      let top_c = lerp_rgb(hex_rgb(0xE0, 0xF2, 0xFE), hex_rgb(0xA5, 0xF3, 0xFC), (t_h * 0.65_f32).clamp(0.0_f32, 1.0_f32));
      lerp_rgb(top_c, hex_rgb(0x22, 0xD3, 0xEE), f * 0.88_f32)
    } else {
      let f = ((v - y_ridge) / (y_bot - y_ridge).max(0.010_f32)).clamp(0.0_f32, 1.0_f32);
      let mid = lerp_rgb(hex_rgb(0x0F, 0x17, 0x2A), hex_rgb(0x1E, 0x1B, 0x4B), (f * 1.35_f32).clamp(0.0_f32, 1.0_f32));
      lerp_rgb(mid, hex_rgb(0x3B, 0x12, 0x54), ((f - 0.35_f32) * 1.5_f32).clamp(0.0_f32, 1.0_f32))
    };
    let ridge = (1.0_f32 - (v - y_ridge).abs() / 0.006_f32).clamp(0.0_f32, 1.0_f32);
    blend_over(&mut hc, hex_rgb(0xE0, 0xF2, 0xFE), ridge * 0.65_f32);
    if v > y_ridge && d_anvil > -0.014_f32 {
      blend_over(&mut hc, hex_rgb(0x22, 0xD3, 0xEE), ((d_anvil + 0.014_f32) / 0.014_f32).clamp(0.0_f32, 1.0_f32) * 0.85_f32);
    }
    hc
  } else if u < 0.335_f32 {
    let tv = ((v - 0.244_f32) / 0.264_f32).clamp(0.0_f32, 1.0_f32);
    let mut c = lerp_rgb(hex_rgb(0x33, 0x41, 0x55), hex_rgb(0x13, 0x1C, 0x31), tv);
    let seam = ((1.0_f32 - (u - 0.306_f32).abs() / 0.005_f32).max(0.0_f32) + (1.0_f32 - (u - 0.334_f32).abs() / 0.005_f32).max(0.0_f32)).min(1.0_f32);
    blend_over(&mut c, hex_rgb(0x05, 0x08, 0x11), seam * 0.82_f32);
    blend_over(&mut c, hex_rgb(0x67, 0xE8, 0xF9), (1.0_f32 - (u - 0.314_f32).abs() / 0.005_f32).clamp(0.0_f32, 1.0_f32) * 0.62_f32);
    c
  } else if is_left_half {
    lerp_rgb(hex_rgb(0x13, 0x20, 0x38), hex_rgb(0x09, 0x10, 0x1F), ((v - 0.205_f32) / 0.625_f32).clamp(0.0_f32, 1.0_f32))
  } else {
    lerp_rgb(hex_rgb(0x1D, 0x16, 0x32), hex_rgb(0x10, 0x0B, 0x1E), ((v - 0.205_f32) / 0.625_f32).clamp(0.0_f32, 1.0_f32))
  };

  if u >= 0.334_f32 && v <= 0.232_f32 {
    let fb = (1.0_f32 - (v - 0.205_f32) / 0.027_f32).clamp(0.0_f32, 1.0_f32);
    let fc = if is_left_half {
      lerp_rgb(hex_rgb(0xE0, 0xF2, 0xFE), hex_rgb(0x22, 0xD3, 0xEE), (1.0_f32 - fb) * 0.85_f32)
    } else {
      lerp_rgb(hex_rgb(0xFE, 0xF0, 0x8A), hex_rgb(0xFF, 0x95, 0x00), (1.0_f32 - fb) * 0.85_f32)
    };
    blend_over(&mut col, fc, fb * fb * 0.92_f32);
  }

  if u >= 0.268_f32 && d_anvil > -0.015_f32 {
    let rt = ((d_anvil + 0.015_f32) / 0.015_f32).clamp(0.0_f32, 1.0_f32);
    if u >= 0.625_f32 && v > 0.212_f32 {
      blend_over(&mut col, lerp_rgb(hex_rgb(0xDB, 0x27, 0x77), hex_rgb(0xF4, 0x72, 0xB6), rt), rt * 0.92_f32);
    } else if u < 0.435_f32 && v > 0.500_f32 {
      blend_over(&mut col, lerp_rgb(hex_rgb(0x02, 0x84, 0xC7), hex_rgb(0x22, 0xD3, 0xEE), rt), rt * 0.82_f32);
    } else if u >= 0.432_f32 && u <= 0.628_f32 && v >= 0.715_f32 {
      blend_over(&mut col, if u < 0.530_f32 { hex_rgb(0x0E, 0xA5, 0xE9) } else { hex_rgb(0xEC, 0x48, 0x99) }, rt * 0.55_f32);
    }
  }

  let gr = (((u - 0.838_f32) / 0.045_f32).powi(2) + ((v - 0.214_f32) / 0.012_f32).powi(2)).sqrt();
  if gr < 1.0_f32 {
    blend_over(&mut col, hex_rgb(0xFE, 0xF9, 0xC3), (1.0_f32 - gr).powi(2) * 0.95_f32);
  }

  if v >= 0.218_f32 && v <= 0.748_f32 && groove_delta.abs() < 0.020_f32 {
    let ga = groove_delta.abs();
    let lip = (1.0_f32 - (ga - 0.010_f32).abs() / 0.008_f32).clamp(0.0_f32, 1.0_f32);
    if groove_delta < -0.005_f32 {
      blend_over(&mut col, hex_rgb(0x22, 0xD3, 0xEE), lip * 0.52_f32);
    } else if groove_delta > 0.005_f32 {
      blend_over(&mut col, hex_rgb(0xFB, 0xBF, 0x24), lip * 0.52_f32);
    }
    blend_over(&mut col, hex_rgb(0x04, 0x07, 0x0F), (1.0_f32 - ga / 0.0095_f32).clamp(0.0_f32, 1.0_f32) * 0.94_f32);
  }

  if include_details && d_anvil < -0.012_f32 {
    apply_circuit_traces(&mut col, u, v, aa_px);
  }

  if u >= 0.375_f32 && u <= 0.605_f32 && v >= 0.250_f32 && v <= 0.585_f32 {
    let d_sh = sd_polygon(&geom.f1, u - 0.008_f32, v - 0.011_f32);
    if d_sh < 0.006_f32 && d_anvil < -0.010_f32 {
      blend_over(&mut col, hex_rgb(0x03, 0x06, 0x0E), ((0.006_f32 - d_sh) / 0.012_f32).clamp(0.0_f32, 1.0_f32) * 0.65_f32);
    }
    let d_f1 = sd_polygon(&geom.f1, u, v);
    let f1_cov = smooth_cov(d_f1, aa_px);
    if f1_cov > 0.0_f32 {
      let tf = ((v - 0.268_f32) / 0.290_f32).clamp(0.0_f32, 1.0_f32);
      let mut fc = if tf < 0.32_f32 {
        lerp_rgb(hex_rgb(0xA5, 0xF3, 0xFC), hex_rgb(0x00, 0xE5, 0xFF), tf / 0.32_f32)
      } else if tf < 0.72_f32 {
        lerp_rgb(hex_rgb(0x00, 0xE5, 0xFF), hex_rgb(0x02, 0x84, 0xC7), (tf - 0.32_f32) / 0.40_f32)
      } else {
        lerp_rgb(hex_rgb(0x02, 0x84, 0xC7), hex_rgb(0x1D, 0x4E, 0xD8), (tf - 0.72_f32) / 0.28_f32)
      };
      if d_f1 > -0.009_f32 {
        let bev = ((d_f1 + 0.009_f32) / 0.009_f32).clamp(0.0_f32, 1.0_f32);
        if sd_polygon(&geom.f1, u + 0.004_f32, v + 0.004_f32) > d_f1 {
          blend_over(&mut fc, hex_rgb(0xEC, 0xFE, 0xFF), bev * 0.82_f32);
        } else {
          blend_over(&mut fc, hex_rgb(0x0C, 0x4A, 0x6E), bev * 0.48_f32);
        }
      }
      blend_over(&mut col, fc, f1_cov);
    }
  }

  if u >= 0.500_f32 && u <= 0.790_f32 && v >= 0.250_f32 && v <= 0.600_f32 {
    let d_sh = sd_polygon(&geom.f2, u - 0.008_f32, v - 0.011_f32);
    if d_sh < 0.006_f32 && d_anvil < -0.010_f32 {
      blend_over(&mut col, hex_rgb(0x05, 0x04, 0x0C), ((0.006_f32 - d_sh) / 0.012_f32).clamp(0.0_f32, 1.0_f32) * 0.65_f32);
    }
    let d_f2 = sd_polygon(&geom.f2, u, v);
    let f2_cov = smooth_cov(d_f2, aa_px);
    if f2_cov > 0.0_f32 {
      let tf = ((v - 0.268_f32) / 0.310_f32).clamp(0.0_f32, 1.0_f32);
      let mut fc = if tf < 0.36_f32 {
        lerp_rgb(hex_rgb(0xFE, 0xF0, 0x8A), hex_rgb(0xFF, 0x95, 0x00), tf / 0.36_f32)
      } else {
        lerp_rgb(hex_rgb(0xFF, 0x95, 0x00), hex_rgb(0xEA, 0x58, 0x0C), (tf - 0.36_f32) / 0.64_f32)
      };
      if d_f2 > -0.009_f32 {
        let bev = ((d_f2 + 0.009_f32) / 0.009_f32).clamp(0.0_f32, 1.0_f32);
        if sd_polygon(&geom.f2, u + 0.004_f32, v + 0.004_f32) > d_f2 {
          blend_over(&mut fc, hex_rgb(0xFE, 0xF9, 0xC3), bev * 0.85_f32);
        } else {
          blend_over(&mut fc, hex_rgb(0xEC, 0x48, 0x99), bev * 0.55_f32);
        }
      }
      blend_over(&mut col, fc, f2_cov);
    }
  }

  [col[0], col[1], col[2], anvil_cov]
}

fn apply_circuit_traces(col: &mut [f32; 3], u: f32, v: f32, aa_px: f32) {
  let cyan_segs: [([f32; 2], [f32; 2]); 7] = [
    ([0.408_f32, 0.560_f32], [0.386_f32, 0.635_f32]), ([0.386_f32, 0.635_f32], [0.326_f32, 0.708_f32]),
    ([0.326_f32, 0.708_f32], [0.326_f32, 0.790_f32]), ([0.436_f32, 0.555_f32], [0.414_f32, 0.650_f32]),
    ([0.414_f32, 0.650_f32], [0.384_f32, 0.722_f32]), ([0.384_f32, 0.722_f32], [0.384_f32, 0.795_f32]),
    ([0.456_f32, 0.356_f32], [0.366_f32, 0.356_f32]),
  ];
  let cyan_nodes = [[0.326_f32, 0.790_f32], [0.384_f32, 0.795_f32], [0.366_f32, 0.356_f32]];
  let mut dc = f32::MAX;
  for &(a, b) in &cyan_segs { dc = dc.min(sd_segment(u, v, a, b) - 0.0030_f32); }
  for &p in &cyan_nodes { dc = dc.min((u - p[0]).hypot(v - p[1]) - 0.0072_f32); }
  if dc < 0.010_f32 {
    let g = ((0.010_f32 - dc) / 0.010_f32).clamp(0.0_f32, 1.0_f32);
    blend_over(col, hex_rgb(0x00, 0xE5, 0xFF), g * g * 0.38_f32);
    let c = smooth_cov(dc, aa_px);
    if c > 0.0_f32 { blend_over(col, hex_rgb(0xA5, 0xF3, 0xFC), c * 0.92_f32); }
  }

  let warm_segs: [([f32; 2], [f32; 2]); 8] = [
    ([0.764_f32, 0.296_f32], [0.824_f32, 0.258_f32]), ([0.824_f32, 0.258_f32], [0.892_f32, 0.258_f32]),
    ([0.684_f32, 0.422_f32], [0.742_f32, 0.376_f32]), ([0.742_f32, 0.376_f32], [0.792_f32, 0.336_f32]),
    ([0.548_f32, 0.576_f32], [0.606_f32, 0.658_f32]), ([0.606_f32, 0.658_f32], [0.678_f32, 0.736_f32]),
    ([0.678_f32, 0.736_f32], [0.678_f32, 0.795_f32]), ([0.618_f32, 0.605_f32], [0.736_f32, 0.752_f32]),
  ];
  let warm_nodes = [[0.892_f32, 0.258_f32], [0.792_f32, 0.336_f32], [0.678_f32, 0.795_f32], [0.736_f32, 0.752_f32]];
  let mut dw = f32::MAX;
  for &(a, b) in &warm_segs { dw = dw.min(sd_segment(u, v, a, b) - 0.0030_f32); }
  for &p in &warm_nodes { dw = dw.min((u - p[0]).hypot(v - p[1]) - 0.0072_f32); }
  if dw < 0.010_f32 {
    let g = ((0.010_f32 - dw) / 0.010_f32).clamp(0.0_f32, 1.0_f32);
    blend_over(col, if v > 0.60_f32 { hex_rgb(0xEC, 0x48, 0x99) } else { hex_rgb(0xFF, 0x95, 0x00) }, g * g * 0.42_f32);
    let c = smooth_cov(dw, aa_px);
    if c > 0.0_f32 { blend_over(col, hex_rgb(0xFE, 0xF0, 0x8A), c * 0.92_f32); }
  }
}

fn sample_sparks(u: f32, v: f32, aa_px: f32) -> [f32; 4] {
  for &(pos, radius, is_cyan) in &SPARKS {
    let d_diamond = ((u - pos[0]).abs() + (v - pos[1]).abs()) * 0.70710677_f32 - radius;
    let cov = smooth_cov(d_diamond, aa_px);
    if cov > 0.0_f32 {
      let core_t = ((-d_diamond) / radius.max(0.001_f32)).clamp(0.0_f32, 1.0_f32);
      let c = if is_cyan {
        lerp_rgb(hex_rgb(0x00, 0xE5, 0xFF), hex_rgb(0xE0, 0xF2, 0xFE), core_t)
      } else {
        lerp_rgb(hex_rgb(0xFF, 0x95, 0x00), hex_rgb(0xFE, 0xF0, 0x8A), core_t)
      };
      return [c[0], c[1], c[2], cov];
    }
  }
  [0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32]
}

fn sd_polygon(poly: &[[f32; 2]], px: f32, py: f32) -> f32 {
  let n = poly.len();
  let (mut d, mut s, mut j) = (f32::MAX, 1.0_f32, n - 1);
  for i in 0..n {
    let ([xi, yi], [xj, yj]) = (poly[i], poly[j]);
    let (ex, ey, wx, wy) = (xj - xi, yj - yi, px - xi, py - yi);
    let denom = ex * ex + ey * ey;
    let h = if denom > 1.0e-12_f32 { ((wx * ex + wy * ey) / denom).clamp(0.0_f32, 1.0_f32) } else { 0.0_f32 };
    let (bx, by) = (wx - ex * h, wy - ey * h);
    d = d.min(bx * bx + by * by);
    let (c1, c2, c3) = (py >= yi, py < yj, ex * wy > ey * wx);
    if (c1 && c2 && c3) || (!c1 && !c2 && !c3) { s = -s; }
    j = i;
  }
  s * d.sqrt()
}

#[inline]
fn sd_segment(px: f32, py: f32, a: [f32; 2], b: [f32; 2]) -> f32 {
  let (vx, vy, ux, uy) = (px - a[0], py - a[1], b[0] - a[0], b[1] - a[1]);
  let denom = ux * ux + uy * uy;
  let h = if denom > 1.0e-12_f32 { ((vx * ux + vy * uy) / denom).clamp(0.0_f32, 1.0_f32) } else { 0.0_f32 };
  (vx - ux * h).hypot(vy - uy * h)
}

#[inline]
fn smooth_cov(sd: f32, aa: f32) -> f32 {
  let w = aa.max(1.0e-4_f32);
  let t = ((w - sd) / (2.0_f32 * w)).clamp(0.0_f32, 1.0_f32);
  t * t * (3.0_f32 - 2.0_f32 * t)
}

#[inline]
fn blend_over(dst: &mut [f32; 3], src: [f32; 3], alpha: f32) {
  let a = alpha.clamp(0.0_f32, 1.0_f32);
  let inv = 1.0_f32 - a;
  dst[0] = dst[0] * inv + src[0] * a;
  dst[1] = dst[1] * inv + src[1] * a;
  dst[2] = dst[2] * inv + src[2] * a;
}

#[inline]
fn hex_rgb(r: u8, g: u8, b: u8) -> [f32; 3] {
  [(r as f32) / 255.0_f32, (g as f32) / 255.0_f32, (b as f32) / 255.0_f32]
}

#[inline]
fn lerp_rgb(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
  let k = t.clamp(0.0_f32, 1.0_f32);
  [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k]
}
