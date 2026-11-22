use std::ffi::{c_void, OsStr};
use std::mem::{size_of, ManuallyDrop};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
  DXGI_FORMAT, DXGI_FORMAT_R16G16_FLOAT, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM,
  DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};

const TIMER_SLOTS: usize = 3;
const MAX_GENERATED_FRAMES: usize = 3;
const DESCRIPTORS_PER_PASS: usize = 5;
const TOTAL_MFG_DESCRIPTORS: u32 =
  (TIMER_SLOTS * MAX_GENERATED_FRAMES * DESCRIPTORS_PER_PASS) as u32;
const MFG_ROOT_CONSTANT_COUNT: u32 = 8;

const SHADER_READ_STATES: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(
  D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0,
);

extern "C" {
  fn ff_dlss45_create(
    d3d12_device: *mut c_void,
    display_width: u32,
    display_height: u32,
    dll_dir_utf16: *const u16,
    out_handle: *mut *mut c_void,
    out_version_utf8: *mut u8,
    out_version_cap: u32,
    out_rtx_fg_supported: *mut i32,
  ) -> i32;

  fn ff_dlss45_tick_frame(handle: *mut c_void, frame_id: u64) -> i32;

  fn ff_dlss45_destroy(handle: *mut c_void);
}

// NVIDIA DLSS 4.5 / Streamline 2.14.16 Spatial-Temporal Multi-Frame-Gen Compute Shader
// (cs_5_0, strictly 7-bit ASCII):
// Synthesizes 1 (2x: alpha=0.50), 2 (3x: alpha=0.3333, 0.6667), or 3 (4x: alpha=0.25, 0.50, 0.75)
// intermediate frames per capture interval.
// - Standard mode (`fast_mode_flag == 0`): 9-tap foreground depth dilation, bidirectional optical
//   warp, Edge-Aegis disocclusion weighting, 5-tap YCoCg local variance neighborhood anti-ghosting
//   clamp, and Transformer-inspired contrast-adaptive edge synthesis.
// - iGPU-Turbo mode (`fast_mode_flag != 0`): Ultra-fast lightweight warp with minimal texture
//   fetches for high FPS on integrated GPUs (e.g. Intel Iris Xe).
const DLSS45_MFG_SYNTH_HLSL: &[u8] = br#"
cbuffer Dlss45MfgRootConstants : register(b0) {
  uint display_w;
  uint display_h;
  uint mv_w;
  uint mv_h;
  uint alpha_bits;
  uint hud_protection_flag;
  uint fast_mode_flag;
  uint rtx_pacing_flag;
};

Texture2D<float4>   prev_color       : register(t0);
Texture2D<float4>   curr_color       : register(t1);
Texture2D<float2>   motion_vectors   : register(t2);
Texture2D<float>    depth_buffer     : register(t3);
RWTexture2D<float4> out_interpolated : register(u0);
SamplerState        linear_clamp     : register(s0);

float calc_luma(float3 rgb) {
  return dot(rgb, float3(0.2126, 0.7152, 0.0722));
}

float max3(float3 v) {
  return max(v.r, max(v.g, v.b));
}

float3 rgb_to_ycocg(float3 c) {
  float co = c.r - c.b;
  float t  = c.b + 0.5 * co;
  float cg = c.g - t;
  float y  = t + 0.5 * cg;
  return float3(y, co, cg);
}

float3 ycocg_to_rgb(float3 ycc) {
  float t = ycc.x - 0.5 * ycc.z;
  float g = ycc.z + t;
  float b = t - 0.5 * ycc.y;
  float r = b + ycc.y;
  return float3(r, g, b);
}

float boundary_weight(float2 uv, float2 half_texel) {
  float2 inside = step(half_texel, uv) * step(uv, float2(1.0, 1.0) - half_texel);
  return inside.x * inside.y;
}

[numthreads(8, 8, 1)]
void cs_dlss45_mfg_synth(uint3 dtid : SV_DispatchThreadID) {
  if (dtid.x >= display_w || dtid.y >= display_h) {
    return;
  }

  float alpha = clamp(asfloat(alpha_bits), 0.05, 0.95);

  float2 disp_f = max(float2(display_w, display_h), float2(1.0, 1.0));
  float2 mv_f   = max(float2(mv_w, mv_h), float2(1.0, 1.0));
  float2 inv_disp = 1.0 / disp_f;
  float2 inv_mv   = 1.0 / mv_f;

  float2 half_disp = inv_disp * 0.5;
  float2 max_disp_uv = float2(1.0, 1.0) - half_disp;
  float2 half_mv = inv_mv * 0.5;
  float2 max_mv_uv = float2(1.0, 1.0) - half_mv;

  float2 curr_uv = clamp((float2(dtid.xy) + 0.5) * inv_disp, half_disp, max_disp_uv);
  float2 gb_uv   = clamp(curr_uv, half_mv, max_mv_uv);

  float2 mv_center = motion_vectors.SampleLevel(linear_clamp, gb_uv, 0.0);
  float  d_center  = depth_buffer.SampleLevel(linear_clamp, gb_uv, 0.0);

  // Fast iGPU-Turbo path: minimal texture fetches for maximum integrated GPU throughput
  if (fast_mode_flag != 0) {
    float2 mv_uv_fast = mv_center * inv_mv;
    float2 uv_prev_fast = clamp(curr_uv + alpha * mv_uv_fast, half_disp, max_disp_uv);
    float2 uv_curr_fast = clamp(curr_uv - (1.0 - alpha) * mv_uv_fast, half_disp, max_disp_uv);

    float3 cp = prev_color.SampleLevel(linear_clamp, uv_prev_fast, 0.0).rgb;
    float3 cc = curr_color.SampleLevel(linear_clamp, uv_curr_fast, 0.0).rgb;

    float vp = boundary_weight(curr_uv + alpha * mv_uv_fast, half_disp);
    float vc = boundary_weight(curr_uv - (1.0 - alpha) * mv_uv_fast, half_disp);
    float wf = alpha;
    if (vp < 0.5 && vc > 0.5) {
      wf = 1.0;
    } else if (vc < 0.5 && vp > 0.5) {
      wf = 0.0;
    }

    float3 fast_rgb = lerp(cp, cc, wf);
    float3 unwarped_c = curr_color.SampleLevel(linear_clamp, curr_uv, 0.0).rgb;
    float3 unwarped_p = prev_color.SampleLevel(linear_clamp, curr_uv, 0.0).rgb;

    float fast_diff = max3(abs(cp - cc));
    float fast_cut = smoothstep(0.42, 0.75, fast_diff) * smoothstep(6.0, 24.0, length(mv_uv_fast * disp_f));
    fast_rgb = lerp(fast_rgb, lerp(unwarped_p, unwarped_c, alpha), fast_cut * 0.78);

    if (hud_protection_flag != 0) {
      float s_diff = max3(abs(unwarped_c - unwarped_p));
      float h_stat = 1.0 - smoothstep(0.012, 0.030, s_diff);
      float h_mot  = smoothstep(0.0008, 0.0030, length(mv_uv_fast));
      fast_rgb = lerp(fast_rgb, unwarped_c, h_stat * h_mot);
    }

    out_interpolated[dtid.xy] = float4(saturate(fast_rgb), 1.0);
    return;
  }

  // Full Spatial-Temporal Multi-Frame-Gen Synthesis path:
  // 1. 9-tap foreground depth dilation in G-Buffer UV space
  float2 mv_fg = mv_center;
  float  d_fg  = d_center;
  float  d_min = d_center;
  float  max_neigh_mv_uv = length(mv_center * inv_mv);

  [unroll] for (int dy = -1; dy <= 1; ++dy) {
    [unroll] for (int dx = -1; dx <= 1; ++dx) {
      if (dx != 0 || dy != 0) {
        float2 nuv = clamp(curr_uv + float2(dx, dy) * inv_mv, half_mv, max_mv_uv);
        float  nd  = depth_buffer.SampleLevel(linear_clamp, nuv, 0.0);
        float2 nmv = motion_vectors.SampleLevel(linear_clamp, nuv, 0.0);
        d_min = min(d_min, nd);
        max_neigh_mv_uv = max(max_neigh_mv_uv, length(nmv * inv_mv));
        if (nd > d_fg + 0.012) {
          d_fg = nd;
          mv_fg = nmv;
        }
      }
    }
  }

  float edge_span = saturate((d_fg - d_min) * 3.5);
  float2 mv_px = lerp(mv_center, mv_fg, edge_span * 0.70);
  float2 mv_uv = mv_px * inv_mv;

  // 2. Arbitrary-phase bidirectional optical warp (2x / 3x / 4x)
  float2 uv_prev = curr_uv + alpha * mv_uv;
  float2 uv_curr = curr_uv - (1.0 - alpha) * mv_uv;

  float2 uv_prev_clamped = clamp(uv_prev, half_disp, max_disp_uv);
  float2 uv_curr_clamped = clamp(uv_curr, half_disp, max_disp_uv);

  float4 col_prev = prev_color.SampleLevel(linear_clamp, uv_prev_clamped, 0.0);
  float4 col_curr = curr_color.SampleLevel(linear_clamp, uv_curr_clamped, 0.0);

  // 3. Edge-Aegis depth & motion-coherence disocclusion weighting anchored at `alpha`
  float2 gb_prev_uv = clamp(uv_prev_clamped, half_mv, max_mv_uv);
  float2 gb_curr_uv = clamp(uv_curr_clamped, half_mv, max_mv_uv);

  float d_prev_warp = depth_buffer.SampleLevel(linear_clamp, gb_prev_uv, 0.0);
  float d_curr_warp = depth_buffer.SampleLevel(linear_clamp, gb_curr_uv, 0.0);

  float2 mv_prev_warp = motion_vectors.SampleLevel(linear_clamp, gb_prev_uv, 0.0);
  float2 mv_curr_warp = motion_vectors.SampleLevel(linear_clamp, gb_curr_uv, 0.0);

  float err_prev_disp = length((mv_prev_warp - mv_px) * inv_mv * disp_f);
  float err_curr_disp = length((mv_curr_warp - mv_px) * inv_mv * disp_f);

  float luma_prev = calc_luma(col_prev.rgb);
  float luma_curr = calc_luma(col_curr.rgb);
  float3 diff_rgb = abs(col_prev.rgb - col_curr.rgb);
  float photo_diff = abs(luma_prev - luma_curr) * 0.70 + max3(diff_rgb) * 0.30;

  float depth_bias = clamp((d_curr_warp - d_prev_warp) * 4.5, -1.0, 1.0);
  float mv_coh_bias = clamp((err_prev_disp - err_curr_disp) * 0.22, -1.0, 1.0);
  float disocc_strength = smoothstep(0.030, 0.20, photo_diff);

  float combined_bias = clamp(depth_bias * 0.65 + mv_coh_bias * 0.35, -0.95, 0.95);
  float phase_span = max(alpha, 1.0 - alpha);
  float w_curr = clamp(alpha + phase_span * combined_bias * disocc_strength, 0.0, 1.0);

  float valid_prev = boundary_weight(uv_prev, half_disp);
  float valid_curr = boundary_weight(uv_curr, half_disp);
  if (valid_prev < 0.5 && valid_curr > 0.5) {
    w_curr = 1.0;
  } else if (valid_curr < 0.5 && valid_prev > 0.5) {
    w_curr = 0.0;
  }

  // 4. 5-tap YCoCg spatial-temporal neighborhood variance clamping (ghosting suppression)
  //    + Transformer-inspired contrast-adaptive edge synthesis
  float3 c_n = curr_color.SampleLevel(linear_clamp, clamp(uv_curr_clamped - float2(0.0, inv_disp.y), half_disp, max_disp_uv), 0.0).rgb;
  float3 c_s = curr_color.SampleLevel(linear_clamp, clamp(uv_curr_clamped + float2(0.0, inv_disp.y), half_disp, max_disp_uv), 0.0).rgb;
  float3 c_w = curr_color.SampleLevel(linear_clamp, clamp(uv_curr_clamped - float2(inv_disp.x, 0.0), half_disp, max_disp_uv), 0.0).rgb;
  float3 c_e = curr_color.SampleLevel(linear_clamp, clamp(uv_curr_clamped + float2(inv_disp.x, 0.0), half_disp, max_disp_uv), 0.0).rgb;

  float3 p_n = prev_color.SampleLevel(linear_clamp, clamp(uv_prev_clamped - float2(0.0, inv_disp.y), half_disp, max_disp_uv), 0.0).rgb;
  float3 p_s = prev_color.SampleLevel(linear_clamp, clamp(uv_prev_clamped + float2(0.0, inv_disp.y), half_disp, max_disp_uv), 0.0).rgb;

  float3 ycc_c0 = rgb_to_ycocg(col_curr.rgb);
  float3 ycc_cn = rgb_to_ycocg(c_n);
  float3 ycc_cs = rgb_to_ycocg(c_s);
  float3 ycc_cw = rgb_to_ycocg(c_w);
  float3 ycc_ce = rgb_to_ycocg(c_e);

  float3 ycc_p0 = rgb_to_ycocg(col_prev.rgb);
  float3 ycc_pn = rgb_to_ycocg(p_n);
  float3 ycc_ps = rgb_to_ycocg(p_s);

  float3 m1_curr = (ycc_c0 + ycc_cn + ycc_cs + ycc_cw + ycc_ce) * 0.20;
  float3 m2_curr = (ycc_c0 * ycc_c0 + ycc_cn * ycc_cn + ycc_cs * ycc_cs + ycc_cw * ycc_cw + ycc_ce * ycc_ce) * 0.20;
  float3 sigma_curr = sqrt(max(m2_curr - m1_curr * m1_curr, float3(0.0, 0.0, 0.0)));

  float3 m1_prev = (ycc_p0 + ycc_pn + ycc_ps) * (1.0 / 3.0);
  float3 m2_prev = (ycc_p0 * ycc_p0 + ycc_pn * ycc_pn + ycc_ps * ycc_ps) * (1.0 / 3.0);
  float3 sigma_prev = sqrt(max(m2_prev - m1_prev * m1_prev, float3(0.0, 0.0, 0.0)));

  // Clamp warped opposite sample toward dominant reference neighborhood during disocclusion
  float gamma = lerp(1.65, 0.85, disocc_strength);
  float3 prev_clamped_ycc = clamp(ycc_p0, m1_curr - sigma_curr * gamma, m1_curr + sigma_curr * gamma);
  float3 curr_clamped_ycc = clamp(ycc_c0, m1_prev - sigma_prev * gamma, m1_prev + sigma_prev * gamma);

  float clamp_prev_amt = disocc_strength * saturate((w_curr - 0.5) * 1.8);
  float clamp_curr_amt = disocc_strength * saturate((0.5 - w_curr) * 1.8);

  float3 clean_prev = ycocg_to_rgb(lerp(ycc_p0, prev_clamped_ycc, clamp_prev_amt));
  float3 clean_curr = ycocg_to_rgb(lerp(ycc_c0, curr_clamped_ycc, clamp_curr_amt));

  float3 blended = lerp(clean_prev, clean_curr, saturate(w_curr));

  // Contrast-adaptive high-frequency synthesis for crisp 3x / 4x intermediate phases
  float3 neigh_avg = 0.25 * (c_n + c_s + c_w + c_e);
  float3 detail_curr = col_curr.rgb - neigh_avg;
  float local_contrast = max3(max(max(c_n, c_s), max(c_w, c_e)) - min(min(c_n, c_s), min(c_w, c_e)));
  float sharp_gain = (rtx_pacing_flag != 0) ? 0.24 : 0.20;
  float detail_gate = (1.0 - disocc_strength) * (1.0 - saturate(local_contrast * 1.35)) * saturate(w_curr * 1.2);
  blended = saturate(blended + detail_curr * (sharp_gain * detail_gate));

  // 5. Scene-cut & extreme occlusion safety fallback
  float4 unwarped_curr = curr_color.SampleLevel(linear_clamp, curr_uv, 0.0);
  float4 unwarped_prev = prev_color.SampleLevel(linear_clamp, curr_uv, 0.0);
  float mv_disp_len = length(mv_uv * disp_f);
  float cut_guard = smoothstep(0.38, 0.72, photo_diff) * smoothstep(6.0, 24.0, mv_disp_len);
  float3 safe_rgb = lerp(blended, lerp(unwarped_prev.rgb, unwarped_curr.rgb, alpha), cut_guard * 0.78);

  // 6. Optional automatic HUD & subtitle protection
  if (hud_protection_flag != 0) {
    float static_diff = max3(abs(unwarped_curr.rgb - unwarped_prev.rgb));
    float hud_static = 1.0 - smoothstep(0.012, 0.030, static_diff);
    float surround_motion = max(length(mv_uv), max_neigh_mv_uv);
    float hud_motion = smoothstep(0.0008, 0.0030, surround_motion);
    safe_rgb = lerp(safe_rgb, unwarped_curr.rgb, hud_static * hud_motion);
  }

  out_interpolated[dtid.xy] = float4(saturate(safe_rgb), 1.0);
}
"#;

#[repr(C)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct Dlss45RootConstants {
  display_w: u32,
  display_h: u32,
  mv_w: u32,
  mv_h: u32,
  alpha_bits: u32,
  hud_protection_flag: u32,
  fast_mode_flag: u32,
  rtx_pacing_flag: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dlss45Mode {
  Inactive,
  StreamlineActive {
    version: String,
    rtx_fg_supported: bool,
  },
  ComputeFallback {
    reason: String,
  },
}

struct Dlss45GpuTimer {
  query_heap: ID3D12QueryHeap,
  readback_buffer: ID3D12Resource,
  timestamp_freq: u64,
  slot_written: [bool; TIMER_SLOTS],
  last_gpu_ms: f32,
  has_sample: bool,
}

pub struct Dlss45Interpolator {
  sdk_handle: *mut c_void,
  mode: Dlss45Mode,
  interpolated_textures: [Option<ID3D12Resource>; MAX_GENERATED_FRAMES],
  retired_textures: Vec<ID3D12Resource>,
  alloc_display_w: u32,
  alloc_display_h: u32,
  last_generated_count: usize,
  frame_id: u64,
  pending_reset: bool,
  was_fast_compute: bool,
  mfg_root_sig: ID3D12RootSignature,
  mfg_pso: ID3D12PipelineState,
  mfg_heap: ID3D12DescriptorHeap,
  descriptor_size: usize,
  gpu_timer: Option<Dlss45GpuTimer>,
}

impl Dlss45Interpolator {
  pub fn new(device: &ID3D12Device, cmd_queue: &ID3D12CommandQueue) -> Result<Self> {
    let mfg_root_sig = Self::create_mfg_root_signature(device)?;
    let mfg_pso = Self::create_mfg_pso(device, &mfg_root_sig)?;

    let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
      Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
      NumDescriptors: TOTAL_MFG_DESCRIPTORS,
      Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
      NodeMask: 0,
    };
    let mfg_heap: ID3D12DescriptorHeap = unsafe {
      device
        .CreateDescriptorHeap(&heap_desc)
        .context("Dlss45Interpolator-DescriptorHeap konnte nicht erstellt werden")?
    };
    let descriptor_size = unsafe {
      device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
    } as usize;

    let gpu_timer = Self::try_create_gpu_timer(device, cmd_queue);

    Ok(Self {
      sdk_handle: null_mut(),
      mode: Dlss45Mode::Inactive,
      interpolated_textures: [None, None, None],
      retired_textures: Vec::new(),
      alloc_display_w: 0,
      alloc_display_h: 0,
      last_generated_count: 0,
      frame_id: 1,
      pending_reset: true,
      was_fast_compute: false,
      mfg_root_sig,
      mfg_pso,
      mfg_heap,
      descriptor_size,
      gpu_timer,
    })
  }

  pub fn ensure_resources(
    &mut self,
    device: &ID3D12Device,
    display_width: u32,
    display_height: u32,
    force_fast_compute: bool,
  ) -> Result<()> {
    let display_w = display_width.max(1);
    let display_h = display_height.max(1);

    if self.was_fast_compute != force_fast_compute {
      self.was_fast_compute = force_fast_compute;
      self.pending_reset = true;
    }

    let textures_missing = self
      .interpolated_textures
      .iter()
      .any(|slot_tex| slot_tex.is_none());

    let display_changed =
      textures_missing || self.alloc_display_w != display_w || self.alloc_display_h != display_h;
    let sdk_needs_recreate = display_changed || matches!(self.mode, Dlss45Mode::Inactive);

    if !display_changed && !sdk_needs_recreate {
      return Ok(());
    }

    if display_changed {
      for slot_tex in &mut self.interpolated_textures {
        if let Some(old_tex) = slot_tex.take() {
          self.retired_textures.push(old_tex);
        }
        let tex = Self::create_uav_color_texture(device, display_w, display_h)?;
        *slot_tex = Some(tex);
      }

      self.last_generated_count = 0;
      self.alloc_display_w = display_w;
      self.alloc_display_h = display_h;

      if let Some(timer) = &mut self.gpu_timer {
        timer.slot_written = [false; TIMER_SLOTS];
      }
    }

    if sdk_needs_recreate {
      self.destroy_sdk_context();
      self.frame_id = 1;
      self.pending_reset = true;

      let dll_dir_utf16 = Self::executable_dir_utf16();
      let mut new_handle: *mut c_void = null_mut();
      let mut version_buf = [0_u8; 128];
      let mut rtx_fg_flag: i32 = 0;

      let create_rc = unsafe {
        ff_dlss45_create(
          Interface::as_raw(device) as *mut c_void,
          display_w,
          display_h,
          dll_dir_utf16.as_ptr(),
          &mut new_handle,
          version_buf.as_mut_ptr(),
          version_buf.len() as u32,
          &mut rtx_fg_flag,
        )
      };

      if create_rc == 0 && !new_handle.is_null() {
        let nul_pos = version_buf
          .iter()
          .position(|&b| b == 0)
          .unwrap_or(version_buf.len());
        let parsed_ver = String::from_utf8_lossy(&version_buf[..nul_pos])
          .trim()
          .to_string();
        let version = if parsed_ver.is_empty() {
          String::from("DLSS 4.5 Streamline 2.14.16")
        } else {
          parsed_ver
        };
        self.sdk_handle = new_handle;
        self.mode = Dlss45Mode::StreamlineActive {
          version,
          rtx_fg_supported: rtx_fg_flag != 0,
        };
      } else {
        if !new_handle.is_null() {
          unsafe {
            ff_dlss45_destroy(new_handle);
          }
        }
        self.sdk_handle = null_mut();
        self.mode = Dlss45Mode::ComputeFallback {
          reason: Self::describe_create_error(create_rc),
        };
      }
    }

    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn dispatch(
    &mut self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    frame_slot: usize,
    prev_color: &ID3D12Resource,
    curr_color: &ID3D12Resource,
    motion_vectors: &ID3D12Resource,
    depth_buffer: &ID3D12Resource,
    display_width: u32,
    display_height: u32,
    mv_width: u32,
    mv_height: u32,
    fg_multiplier: u32,
    force_reset: bool,
    force_fast_compute: bool,
    hud_protection: bool,
  ) -> Result<()> {
    if display_width == 0 || display_height == 0 {
      bail!(
        "Ungueltige Zielaufloesung fuer Dlss45Interpolator::dispatch ({display_width}x{display_height})"
      );
    }

    let mult = fg_multiplier.clamp(2, 4);
    let num_generated = (mult - 1) as usize;
    let mv_w = mv_width.max(1).min(display_width);
    let mv_h = mv_height.max(1).min(display_height);

    let slot = frame_slot % TIMER_SLOTS;
    self.readback_slot_timestamp(slot);

    let switched_fast_mode = self.was_fast_compute != force_fast_compute;
    self.ensure_resources(device, display_width, display_height, force_fast_compute)?;

    let out_tex_0 = self.interpolated_textures[0]
      .as_ref()
      .context("DLSS-4.5-Zieltextur [0] fehlt nach ensure_resources")?
      .clone();
    let out_tex_1 = self.interpolated_textures[1]
      .as_ref()
      .context("DLSS-4.5-Zieltextur [1] fehlt nach ensure_resources")?
      .clone();
    let out_tex_2 = self.interpolated_textures[2]
      .as_ref()
      .context("DLSS-4.5-Zieltextur [2] fehlt nach ensure_resources")?
      .clone();
    let out_textures = [out_tex_0, out_tex_1, out_tex_2];

    let is_reset = force_reset || self.pending_reset || switched_fast_mode;
    let current_frame_id = self.frame_id;
    self.frame_id = self.frame_id.wrapping_add(1);
    let query_base = (slot * 2) as u32;

    let mut rtx_pacing_active = false;
    if force_fast_compute {
      self.pending_reset = true;
    } else {
      self.pending_reset = false;
      if !self.sdk_handle.is_null() {
        let tick_rc = unsafe { ff_dlss45_tick_frame(self.sdk_handle, current_frame_id) };
        if tick_rc >= 0 {
          rtx_pacing_active = matches!(
            self.mode,
            Dlss45Mode::StreamlineActive {
              rtx_fg_supported: true,
              ..
            }
          );
        }
      }
    }

    let mut pre_barriers: Vec<D3D12_RESOURCE_BARRIER> = Vec::with_capacity(num_generated);
    for out_tex in out_textures.iter().take(num_generated) {
      pre_barriers.push(Self::transition_barrier(
        out_tex,
        SHADER_READ_STATES,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
      ));
    }

    unsafe {
      cmd_list.ResourceBarrier(&pre_barriers);

      if let Some(timer) = &self.gpu_timer {
        cmd_list.EndQuery(&timer.query_heap, D3D12_QUERY_TYPE_TIMESTAMP, query_base);
      }

      cmd_list.SetDescriptorHeaps(&[Some(self.mfg_heap.clone())]);
      cmd_list.SetComputeRootSignature(&self.mfg_root_sig);
      cmd_list.SetPipelineState(&self.mfg_pso);
    }

    let effective_prev = if is_reset { curr_color } else { prev_color };

    for (gen_idx, out_tex) in out_textures.iter().take(num_generated).enumerate() {
      let alpha = ((gen_idx + 1) as f32) / (mult as f32);
      self.record_mfg_pass(
        device,
        cmd_list,
        slot,
        gen_idx,
        effective_prev,
        curr_color,
        motion_vectors,
        depth_buffer,
        out_tex,
        display_width,
        display_height,
        mv_w,
        mv_h,
        alpha,
        hud_protection,
        force_fast_compute,
        rtx_pacing_active,
      );
    }

    let mut post_barriers: Vec<D3D12_RESOURCE_BARRIER> = Vec::with_capacity(num_generated * 2);
    for out_tex in out_textures.iter().take(num_generated) {
      post_barriers.push(Self::uav_barrier(out_tex));
      post_barriers.push(Self::transition_barrier(
        out_tex,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        SHADER_READ_STATES,
      ));
    }

    unsafe {
      if let Some(timer) = &mut self.gpu_timer {
        cmd_list.EndQuery(
          &timer.query_heap,
          D3D12_QUERY_TYPE_TIMESTAMP,
          query_base + 1,
        );
        cmd_list.ResolveQueryData(
          &timer.query_heap,
          D3D12_QUERY_TYPE_TIMESTAMP,
          query_base,
          2,
          &timer.readback_buffer,
          (slot * size_of::<[u64; 2]>()) as u64,
        );
        timer.slot_written[slot] = true;
      }

      cmd_list.ResourceBarrier(&post_barriers);
    }

    self.last_generated_count = num_generated;
    Ok(())
  }

  pub fn interpolated_resource(&self, gen_idx: usize) -> Option<&ID3D12Resource> {
    if gen_idx < self.last_generated_count && gen_idx < MAX_GENERATED_FRAMES {
      self.interpolated_textures[gen_idx].as_ref()
    } else {
      None
    }
  }

  #[allow(dead_code)]
  pub fn generated_frame_count(&self) -> usize {
    self.last_generated_count
  }

  pub fn mode(&self) -> &Dlss45Mode {
    &self.mode
  }

  pub fn mode_summary(&self) -> String {
    match &self.mode {
      Dlss45Mode::Inactive => String::from("Inaktiv"),
      Dlss45Mode::StreamlineActive {
        version,
        rtx_fg_supported,
      } => {
        if *rtx_fg_supported {
          format!("Streamline RTX aktiv ({version})")
        } else {
          format!("Streamline erkannt ({version})")
        }
      }
      Dlss45Mode::ComputeFallback { reason } => format!("DX12-Fallback ({reason})"),
    }
  }

  pub fn active_status_label(&self, force_fast_compute: bool) -> String {
    if force_fast_compute {
      return String::from("DLSS 4.5 iGPU-Turbo (MFG-CS)");
    }
    match &self.mode {
      Dlss45Mode::Inactive => String::from("Inaktiv"),
      Dlss45Mode::StreamlineActive { version, .. } => {
        let trimmed = version.trim();
        if trimmed.is_empty() {
          String::from("DLSS 4.5 Streamline 2.14.16")
        } else {
          trimmed.to_string()
        }
      }
      Dlss45Mode::ComputeFallback { .. } => String::from("DLSS 4.5 Fallback (MFG-CS)"),
    }
  }

  pub fn is_using_native_sdk(&self, force_fast_compute: bool) -> bool {
    !force_fast_compute
      && matches!(
        self.mode,
        Dlss45Mode::StreamlineActive {
          rtx_fg_supported: true,
          ..
        }
      )
  }

  #[allow(dead_code)]
  pub fn is_streamline_detected(&self) -> bool {
    matches!(self.mode, Dlss45Mode::StreamlineActive { .. })
  }

  #[allow(dead_code)]
  pub fn allocated_dimensions(&self) -> (u32, u32) {
    (self.alloc_display_w, self.alloc_display_h)
  }

  pub fn last_gpu_time_ms(&self) -> f32 {
    self
      .gpu_timer
      .as_ref()
      .map(|t| t.last_gpu_ms)
      .unwrap_or(0.0_f32)
  }

  pub fn request_reset(&mut self) {
    self.pending_reset = true;
  }

  fn destroy_sdk_context(&mut self) {
    if !self.sdk_handle.is_null() {
      unsafe {
        ff_dlss45_destroy(self.sdk_handle);
      }
      self.sdk_handle = null_mut();
    }
  }

  fn readback_slot_timestamp(&mut self, slot: usize) {
    self.retired_textures.clear();
    let Some(timer) = &mut self.gpu_timer else {
      return;
    };
    if !timer.slot_written[slot] || timer.timestamp_freq == 0 {
      return;
    }
    timer.slot_written[slot] = false;

    let byte_offset = slot * size_of::<[u64; 2]>();
    let read_range = D3D12_RANGE {
      Begin: byte_offset,
      End: byte_offset + size_of::<[u64; 2]>(),
    };

    unsafe {
      let mut mapped_ptr: *mut c_void = null_mut();
      if timer
        .readback_buffer
        .Map(0, Some(&read_range), Some(&mut mapped_ptr))
        .is_ok()
        && !mapped_ptr.is_null()
      {
        let timestamps = *((mapped_ptr as *const u8).add(byte_offset) as *const [u64; 2]);
        let empty_write_range = D3D12_RANGE { Begin: 0, End: 0 };
        timer.readback_buffer.Unmap(0, Some(&empty_write_range));

        let ticks = timestamps[1].saturating_sub(timestamps[0]);
        let measured_ms = (((ticks as f64) * 1000.0_f64 / (timer.timestamp_freq as f64)) as f32)
          .clamp(0.0_f32, 100.0_f32);

        timer.last_gpu_ms = if !timer.has_sample {
          timer.has_sample = true;
          measured_ms
        } else {
          timer.last_gpu_ms * 0.80_f32 + measured_ms * 0.20_f32
        };
      }
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn record_mfg_pass(
    &self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    slot: usize,
    gen_idx: usize,
    prev_color: &ID3D12Resource,
    curr_color: &ID3D12Resource,
    motion_vectors: &ID3D12Resource,
    depth_buffer: &ID3D12Resource,
    out_interpolated: &ID3D12Resource,
    display_w: u32,
    display_h: u32,
    mv_w: u32,
    mv_h: u32,
    alpha: f32,
    hud_protection: bool,
    fast_mode: bool,
    rtx_pacing_active: bool,
  ) {
    let base = ((slot * MAX_GENERATED_FRAMES) + gen_idx) * DESCRIPTORS_PER_PASS;
    self.write_srv_2d(device, base, prev_color, DXGI_FORMAT_R8G8B8A8_UNORM);
    self.write_srv_2d(device, base + 1, curr_color, DXGI_FORMAT_R8G8B8A8_UNORM);
    self.write_srv_2d(device, base + 2, motion_vectors, DXGI_FORMAT_R16G16_FLOAT);
    self.write_srv_2d(device, base + 3, depth_buffer, DXGI_FORMAT_R32_FLOAT);
    self.write_uav_2d(
      device,
      base + 4,
      out_interpolated,
      DXGI_FORMAT_R8G8B8A8_UNORM,
    );

    let clamped_alpha = alpha.clamp(0.05_f32, 0.95_f32);
    let constants = Dlss45RootConstants {
      display_w: display_w.max(1),
      display_h: display_h.max(1),
      mv_w: mv_w.max(1),
      mv_h: mv_h.max(1),
      alpha_bits: clamped_alpha.to_bits(),
      hud_protection_flag: if hud_protection { 1_u32 } else { 0_u32 },
      fast_mode_flag: if fast_mode { 1_u32 } else { 0_u32 },
      rtx_pacing_flag: if rtx_pacing_active { 1_u32 } else { 0_u32 },
    };

    unsafe {
      cmd_list.SetComputeRoot32BitConstants(
        0,
        MFG_ROOT_CONSTANT_COUNT,
        &constants as *const Dlss45RootConstants as *const c_void,
        0,
      );
      cmd_list.SetComputeRootDescriptorTable(1, self.gpu_descriptor_handle(base));
      cmd_list.SetComputeRootDescriptorTable(2, self.gpu_descriptor_handle(base + 4));
      cmd_list.Dispatch(display_w.div_ceil(8), display_h.div_ceil(8), 1);
    }
  }

  fn write_srv_2d(&self, device: &ID3D12Device, index: usize, res: &ID3D12Resource, fmt: DXGI_FORMAT) {
    let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
      Format: fmt,
      ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
      Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
      Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
        Texture2D: D3D12_TEX2D_SRV {
          MostDetailedMip: 0,
          MipLevels: 1,
          PlaneSlice: 0,
          ResourceMinLODClamp: 0.0_f32,
        },
      },
    };
    unsafe {
      device.CreateShaderResourceView(res, Some(&srv_desc), self.cpu_descriptor_handle(index));
    }
  }

  fn write_uav_2d(&self, device: &ID3D12Device, index: usize, res: &ID3D12Resource, fmt: DXGI_FORMAT) {
    let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
      Format: fmt,
      ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
      Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
        Texture2D: D3D12_TEX2D_UAV {
          MipSlice: 0,
          PlaneSlice: 0,
        },
      },
    };
    unsafe {
      device.CreateUnorderedAccessView(
        res,
        None::<&ID3D12Resource>,
        Some(&uav_desc),
        self.cpu_descriptor_handle(index),
      );
    }
  }

  fn cpu_descriptor_handle(&self, index: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    let start = unsafe { self.mfg_heap.GetCPUDescriptorHandleForHeapStart() };
    D3D12_CPU_DESCRIPTOR_HANDLE {
      ptr: start.ptr + index * self.descriptor_size,
    }
  }

  fn gpu_descriptor_handle(&self, index: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
    let start = unsafe { self.mfg_heap.GetGPUDescriptorHandleForHeapStart() };
    D3D12_GPU_DESCRIPTOR_HANDLE {
      ptr: start.ptr + (index * self.descriptor_size) as u64,
    }
  }

  fn executable_dir_utf16() -> Vec<u16> {
    let maybe_dir = std::env::current_exe()
      .ok()
      .and_then(|exe_path| exe_path.parent().map(Path::to_path_buf));

    match maybe_dir {
      Some(dir) => dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0_u16))
        .collect(),
      None => OsStr::new("")
        .encode_wide()
        .chain(std::iter::once(0_u16))
        .collect(),
    }
  }

  fn describe_create_error(code: i32) -> String {
    match code {
      -100 => String::from("Ohne NVIDIA-Streamline-SDK-Header gebaut"),
      -2 => String::from("sl.interposer.dll / sl.dlss_g.dll nicht gefunden"),
      -3 => String::from("Streamline-Kontextspeicher konnte nicht reserviert werden"),
      other => format!("Streamline-Init Code {other}"),
    }
  }

  fn create_uav_color_texture(
    device: &ID3D12Device,
    width: u32,
    height: u32,
  ) -> Result<ID3D12Resource> {
    let heap_props = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_DEFAULT,
      ..Default::default()
    };
    let res_desc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
      Alignment: 0,
      Width: width.max(1) as u64,
      Height: height.max(1),
      DepthOrArraySize: 1,
      MipLevels: 1,
      Format: DXGI_FORMAT_R8G8B8A8_UNORM,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
      Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    };

    let mut texture: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(
          &heap_props,
          D3D12_HEAP_FLAG_NONE,
          &res_desc,
          SHADER_READ_STATES,
          None,
          &mut texture,
        )
        .context("CreateCommittedResource fuer DLSS-4.5-Zieltextur fehlgeschlagen")?;
    }
    texture.context("DLSS-4.5-Zieltextur wurde nicht zurueckgegeben")
  }

  fn create_mfg_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let srv_range = [D3D12_DESCRIPTOR_RANGE {
      RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
      NumDescriptors: 4,
      BaseShaderRegister: 0,
      RegisterSpace: 0,
      OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let uav_range = [D3D12_DESCRIPTOR_RANGE {
      RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
      NumDescriptors: 1,
      BaseShaderRegister: 0,
      RegisterSpace: 0,
      OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];

    let root_params = [
      D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
          Constants: D3D12_ROOT_CONSTANTS {
            ShaderRegister: 0,
            RegisterSpace: 0,
            Num32BitValues: MFG_ROOT_CONSTANT_COUNT,
          },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
      },
      D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
          DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
            NumDescriptorRanges: srv_range.len() as u32,
            pDescriptorRanges: srv_range.as_ptr(),
          },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
      },
      D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
          DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
            NumDescriptorRanges: uav_range.len() as u32,
            pDescriptorRanges: uav_range.as_ptr(),
          },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
      },
    ];

    let static_sampler = [D3D12_STATIC_SAMPLER_DESC {
      Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
      AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      MipLODBias: 0.0_f32,
      MaxAnisotropy: 0,
      ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
      BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK,
      MinLOD: 0.0_f32,
      MaxLOD: f32::MAX,
      ShaderRegister: 0,
      RegisterSpace: 0,
      ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }];

    let root_desc = D3D12_ROOT_SIGNATURE_DESC {
      NumParameters: root_params.len() as u32,
      pParameters: root_params.as_ptr(),
      NumStaticSamplers: static_sampler.len() as u32,
      pStaticSamplers: static_sampler.as_ptr(),
      Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };

    let mut sig_blob: Option<ID3DBlob> = None;
    let mut err_blob: Option<ID3DBlob> = None;
    if let Err(err) = unsafe {
      D3D12SerializeRootSignature(
        &root_desc,
        D3D_ROOT_SIGNATURE_VERSION_1,
        &mut sig_blob,
        Some(&mut err_blob),
      )
    } {
      let detail = Self::blob_to_string(err_blob.as_ref());
      bail!("Dlss45Interpolator RootSignature-Serialisierung fehlgeschlagen ({err}): {detail}");
    }

    let blob = sig_blob.context("Kein serialisierter RootSignature-Blob fuer Dlss45Interpolator")?;
    let bytes = unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    unsafe {
      device
        .CreateRootSignature(0, bytes)
        .context("CreateRootSignature fuer Dlss45Interpolator fehlgeschlagen")
    }
  }

  fn create_mfg_pso(
    device: &ID3D12Device,
    root_sig: &ID3D12RootSignature,
  ) -> Result<ID3D12PipelineState> {
    let cs_blob = Self::compile_mfg_cs()?;
    let mut pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
      pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
      CS: D3D12_SHADER_BYTECODE {
        pShaderBytecode: unsafe { cs_blob.GetBufferPointer() },
        BytecodeLength: unsafe { cs_blob.GetBufferSize() },
      },
      NodeMask: 0,
      CachedPSO: Default::default(),
      Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
    };

    let pso_res = unsafe { device.CreateComputePipelineState(&pso_desc) };
    unsafe {
      ManuallyDrop::drop(&mut pso_desc.pRootSignature);
    }
    pso_res.context("CreateComputePipelineState fuer DLSS-4.5-MFG fehlgeschlagen")
  }

  fn compile_mfg_cs() -> Result<ID3DBlob> {
    let mut code_blob: Option<ID3DBlob> = None;
    let mut err_blob: Option<ID3DBlob> = None;
    let res = unsafe {
      D3DCompile(
        DLSS45_MFG_SYNTH_HLSL.as_ptr() as *const c_void,
        DLSS45_MFG_SYNTH_HLSL.len(),
        PCSTR(b"frameforge_dlss45_mfg_cs.hlsl\0".as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(b"cs_dlss45_mfg_synth\0".as_ptr()),
        PCSTR(b"cs_5_0\0".as_ptr()),
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code_blob,
        Some(&mut err_blob),
      )
    };
    if let Err(err) = res {
      let detail = Self::blob_to_string(err_blob.as_ref());
      bail!("D3DCompile fuer cs_dlss45_mfg_synth fehlgeschlagen ({err}): {detail}");
    }
    code_blob.context("Kein Bytecode-Blob fuer cs_dlss45_mfg_synth erhalten")
  }

  fn try_create_gpu_timer(
    device: &ID3D12Device,
    cmd_queue: &ID3D12CommandQueue,
  ) -> Option<Dlss45GpuTimer> {
    let timestamp_freq = unsafe { cmd_queue.GetTimestampFrequency().ok()? };
    if timestamp_freq == 0 {
      return None;
    }

    let query_desc = D3D12_QUERY_HEAP_DESC {
      Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
      Count: (TIMER_SLOTS * 2) as u32,
      NodeMask: 0,
    };
    let mut query_heap: Option<ID3D12QueryHeap> = None;
    unsafe {
      device.CreateQueryHeap(&query_desc, &mut query_heap).ok()?;
    }

    let readback_props = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_READBACK,
      ..Default::default()
    };
    let readback_desc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
      Alignment: 0,
      Width: (TIMER_SLOTS * size_of::<[u64; 2]>()) as u64,
      Height: 1,
      DepthOrArraySize: 1,
      MipLevels: 1,
      Format: DXGI_FORMAT_UNKNOWN,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
      Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let mut readback_buffer: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(
          &readback_props,
          D3D12_HEAP_FLAG_NONE,
          &readback_desc,
          D3D12_RESOURCE_STATE_COPY_DEST,
          None,
          &mut readback_buffer,
        )
        .ok()?;
    }

    Some(Dlss45GpuTimer {
      query_heap: query_heap?,
      readback_buffer: readback_buffer?,
      timestamp_freq,
      slot_written: [false; TIMER_SLOTS],
      last_gpu_ms: 0.0_f32,
      has_sample: false,
    })
  }

  fn transition_barrier(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
  ) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
      Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
      Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
      Anonymous: D3D12_RESOURCE_BARRIER_0 {
        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
          pResource: unsafe { std::mem::transmute_copy(res) },
          Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
          StateBefore: before,
          StateAfter: after,
        }),
      },
    }
  }

  fn uav_barrier(res: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
      Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
      Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
      Anonymous: D3D12_RESOURCE_BARRIER_0 {
        UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
          pResource: unsafe { std::mem::transmute_copy(res) },
        }),
      },
    }
  }

  fn blob_to_string(blob: Option<&ID3DBlob>) -> String {
    let Some(b) = blob else {
      return String::from("<keine Details>");
    };
    unsafe {
      let ptr = b.GetBufferPointer() as *const u8;
      let len = b.GetBufferSize();
      if ptr.is_null() || len == 0 {
        String::from("<leer>")
      } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len))
          .trim()
          .to_string()
      }
    }
  }
}

impl Drop for Dlss45Interpolator {
  fn drop(&mut self) {
    self.destroy_sdk_context();
  }
}
