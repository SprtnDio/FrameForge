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
const DESCRIPTORS_PER_SLOT: usize = 5;
const TOTAL_FALLBACK_DESCRIPTORS: u32 = (TIMER_SLOTS * DESCRIPTORS_PER_SLOT) as u32;
const FALLBACK_ROOT_CONSTANT_COUNT: u32 = 8;

const SHADER_READ_STATES: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(
  D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0,
);

extern "C" {
  fn ff_fsr3_create(
    d3d12_device: *mut c_void,
    display_width: u32,
    display_height: u32,
    mv_width: u32,
    mv_height: u32,
    dll_dir_utf16: *const u16,
    out_handle: *mut *mut c_void,
    out_version_utf8: *mut u8,
    out_version_cap: u32,
  ) -> i32;

  fn ff_fsr3_dispatch(
    handle: *mut c_void,
    cmd_list: *mut c_void,
    curr_color_res: *mut c_void,
    depth_res: *mut c_void,
    motion_vectors_res: *mut c_void,
    out_interpolated_res: *mut c_void,
    display_width: u32,
    display_height: u32,
    mv_width: u32,
    mv_height: u32,
    delta_time_ms: f32,
    frame_id: u64,
    reset: i32,
  ) -> i32;

  fn ff_fsr3_destroy(handle: *mut c_void);
}

// Standalone DX12 Compute Interpolator & iGPU Turbo Shader (cs_5_0, strictly 7-bit ASCII):
// Synthesizes intermediate frame (N - 0.5) at t = 0.5 using normalized UV sampling across
// prev_color, curr_color, motion_vectors, and depth_buffer. Converts pixel motion vectors
// via `mv_uv = mv / float2(mv_w, mv_h)` so both 100% Native and 50% Flow-Scale G-Buffers
// warp with identical accuracy, and optionally protects static HUD / subtitle elements.
const FALLBACK_INTERP_HLSL: &[u8] = br#"
cbuffer InterpRootConstants : register(b0) {
  uint display_w;
  uint display_h;
  uint mv_w;
  uint mv_h;
  uint hud_protection_flag;
  uint _pad0;
  uint _pad1;
  uint _pad2;
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

float boundary_weight(float2 uv, float2 half_texel) {
  float2 inside = step(half_texel, uv) * step(uv, float2(1.0, 1.0) - half_texel);
  return inside.x * inside.y;
}

[numthreads(8, 8, 1)]
void cs_fsr3_fallback(uint3 dtid : SV_DispatchThreadID) {
  if (dtid.x >= display_w || dtid.y >= display_h) {
    return;
  }

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

  // 1. Resolution-independent foreground depth dilation in G-Buffer UV space
  float2 mv_center = motion_vectors.SampleLevel(linear_clamp, gb_uv, 0.0);
  float  d_center  = depth_buffer.SampleLevel(linear_clamp, gb_uv, 0.0);
  float2 mv_fg     = mv_center;
  float  d_fg      = d_center;
  float  d_min     = d_center;
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
  float2 mv_px = lerp(mv_center, mv_fg, edge_span * 0.65);
  float2 mv_uv = mv_px * inv_mv;

  // 2. Bidirectional motion warp at t = 0.5:
  //    uv_prev = curr_uv + 0.5 * mv_uv, uv_curr = curr_uv - 0.5 * mv_uv
  float2 uv_prev = curr_uv + 0.5 * mv_uv;
  float2 uv_curr = curr_uv - 0.5 * mv_uv;

  float2 uv_prev_clamped = clamp(uv_prev, half_disp, max_disp_uv);
  float2 uv_curr_clamped = clamp(uv_curr, half_disp, max_disp_uv);

  float4 col_prev = prev_color.SampleLevel(linear_clamp, uv_prev_clamped, 0.0);
  float4 col_curr = curr_color.SampleLevel(linear_clamp, uv_curr_clamped, 0.0);

  // 3. Edge-Aegis disocclusion & photometric consistency weighting
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
  float disocc_strength = smoothstep(0.035, 0.22, photo_diff);

  float w_curr = 0.5 + 0.5 * clamp(depth_bias * 0.65 + mv_coh_bias * 0.35, -0.92, 0.92) * disocc_strength;

  float valid_prev = boundary_weight(uv_prev, half_disp);
  float valid_curr = boundary_weight(uv_curr, half_disp);
  if (valid_prev < 0.5 && valid_curr > 0.5) {
    w_curr = 1.0;
  } else if (valid_curr < 0.5 && valid_prev > 0.5) {
    w_curr = 0.0;
  }

  float3 blended = lerp(col_prev.rgb, col_curr.rgb, saturate(w_curr));

  // 4. Scene-cut & extreme occlusion safety fallback using normalized UV fetches
  float4 unwarped_curr = curr_color.SampleLevel(linear_clamp, curr_uv, 0.0);
  float4 unwarped_prev = prev_color.SampleLevel(linear_clamp, curr_uv, 0.0);
  float mv_disp_len = length(mv_uv * disp_f);
  float cut_guard = smoothstep(0.38, 0.72, photo_diff) * smoothstep(6.0, 24.0, mv_disp_len);
  float3 safe_rgb = lerp(blended, lerp(unwarped_prev.rgb, unwarped_curr.rgb, 0.65), cut_guard * 0.75);

  // 5. Optional automatic HUD & subtitle protection
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
struct FallbackRootConstants {
  display_w: u32,
  display_h: u32,
  mv_w: u32,
  mv_h: u32,
  hud_protection_flag: u32,
  _pad0: u32,
  _pad1: u32,
  _pad2: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fsr3Mode {
  Inactive,
  NativeSdk { version: String },
  ComputeFallback { reason: String },
}

struct Fsr3GpuTimer {
  query_heap: ID3D12QueryHeap,
  readback_buffer: ID3D12Resource,
  timestamp_freq: u64,
  slot_written: [bool; TIMER_SLOTS],
  last_gpu_ms: f32,
  has_sample: bool,
}

pub struct Fsr3Interpolator {
  sdk_handle: *mut c_void,
  mode: Fsr3Mode,
  interpolated_texture: Option<ID3D12Resource>,
  retired_textures: Vec<ID3D12Resource>,
  alloc_display_w: u32,
  alloc_display_h: u32,
  alloc_mv_w: u32,
  alloc_mv_h: u32,
  frame_id: u64,
  pending_reset: bool,
  was_fast_compute: bool,
  fallback_root_sig: ID3D12RootSignature,
  fallback_pso: ID3D12PipelineState,
  fallback_heap: ID3D12DescriptorHeap,
  descriptor_size: usize,
  gpu_timer: Option<Fsr3GpuTimer>,
}

impl Fsr3Interpolator {
  pub fn new(device: &ID3D12Device, cmd_queue: &ID3D12CommandQueue) -> Result<Self> {
    let fallback_root_sig = Self::create_fallback_root_signature(device)?;
    let fallback_pso = Self::create_fallback_pso(device, &fallback_root_sig)?;

    let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
      Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
      NumDescriptors: TOTAL_FALLBACK_DESCRIPTORS,
      Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
      NodeMask: 0,
    };
    let fallback_heap: ID3D12DescriptorHeap = unsafe {
      device
        .CreateDescriptorHeap(&heap_desc)
        .context("Fsr3Interpolator-DescriptorHeap konnte nicht erstellt werden")?
    };
    let descriptor_size = unsafe {
      device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
    } as usize;

    let gpu_timer = Self::try_create_gpu_timer(device, cmd_queue);

    Ok(Self {
      sdk_handle: null_mut(),
      mode: Fsr3Mode::Inactive,
      interpolated_texture: None,
      retired_textures: Vec::new(),
      alloc_display_w: 0,
      alloc_display_h: 0,
      alloc_mv_w: 0,
      alloc_mv_h: 0,
      frame_id: 1,
      pending_reset: true,
      was_fast_compute: false,
      fallback_root_sig,
      fallback_pso,
      fallback_heap,
      descriptor_size,
      gpu_timer,
    })
  }

  pub fn ensure_resources(
    &mut self,
    device: &ID3D12Device,
    display_width: u32,
    display_height: u32,
    mv_width: u32,
    mv_height: u32,
    force_fast_compute: bool,
  ) -> Result<()> {
    let display_w = display_width.max(1);
    let display_h = display_height.max(1);
    let mv_w = mv_width.max(1).min(display_w);
    let mv_h = mv_height.max(1).min(display_h);

    if self.was_fast_compute != force_fast_compute {
      self.was_fast_compute = force_fast_compute;
      self.pending_reset = true;
    }

    let display_changed = self.interpolated_texture.is_none()
      || self.alloc_display_w != display_w
      || self.alloc_display_h != display_h;
    let mv_changed = self.alloc_mv_w != mv_w || self.alloc_mv_h != mv_h;
    let sdk_needs_recreate =
      display_changed || mv_changed || matches!(self.mode, Fsr3Mode::Inactive);

    if !display_changed && !sdk_needs_recreate {
      return Ok(());
    }

    if display_changed {
      if let Some(old_tex) = self.interpolated_texture.take() {
        self.retired_textures.push(old_tex);
      }
      let tex = Self::create_interpolated_texture(device, display_w, display_h)?;
      self.interpolated_texture = Some(tex);
      self.alloc_display_w = display_w;
      self.alloc_display_h = display_h;
      if let Some(timer) = &mut self.gpu_timer {
        timer.slot_written = [false; TIMER_SLOTS];
      }
    }

    if sdk_needs_recreate {
      self.destroy_sdk_context();
      self.alloc_mv_w = mv_w;
      self.alloc_mv_h = mv_h;
      self.frame_id = 1;
      self.pending_reset = true;

      let dll_dir_utf16 = Self::executable_dir_utf16();
      let mut new_handle: *mut c_void = null_mut();
      let mut version_buf = [0_u8; 96];

      let create_rc = unsafe {
        ff_fsr3_create(
          Interface::as_raw(device) as *mut c_void,
          display_w,
          display_h,
          mv_w,
          mv_h,
          dll_dir_utf16.as_ptr(),
          &mut new_handle,
          version_buf.as_mut_ptr(),
          version_buf.len() as u32,
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
          String::from("AMD FSR 3.1 (DX12)")
        } else {
          parsed_ver
        };
        self.sdk_handle = new_handle;
        self.mode = Fsr3Mode::NativeSdk { version };
      } else {
        if !new_handle.is_null() {
          unsafe {
            ff_fsr3_destroy(new_handle);
          }
        }
        self.sdk_handle = null_mut();
        self.mode = Fsr3Mode::ComputeFallback {
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
    delta_time_ms: f32,
    force_reset: bool,
    force_fast_compute: bool,
    hud_protection: bool,
  ) -> Result<()> {
    if display_width == 0 || display_height == 0 {
      bail!(
        "Ungueltige Zielaufloesung fuer Fsr3Interpolator::dispatch ({display_width}x{display_height})"
      );
    }

    let mv_w = mv_width.max(1).min(display_width);
    let mv_h = mv_height.max(1).min(display_height);

    let slot = frame_slot % TIMER_SLOTS;
    self.readback_slot_timestamp(slot);

    let switched_fast_mode = self.was_fast_compute != force_fast_compute;
    self.ensure_resources(
      device,
      display_width,
      display_height,
      mv_w,
      mv_h,
      force_fast_compute,
    )?;

    let interp_tex = self
      .interpolated_texture
      .as_ref()
      .context("Interpolierte Zieltextur fehlt nach ensure_resources")?
      .clone();

    let is_reset = force_reset || self.pending_reset || switched_fast_mode;
    let current_frame_id = self.frame_id;
    self.frame_id = self.frame_id.wrapping_add(1);
    let query_base = (slot * 2) as u32;

    unsafe {
      cmd_list.ResourceBarrier(&[Self::transition_barrier(
        &interp_tex,
        SHADER_READ_STATES,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
      )]);

      if let Some(timer) = &self.gpu_timer {
        cmd_list.EndQuery(&timer.query_heap, D3D12_QUERY_TYPE_TIMESTAMP, query_base);
      }
    }

    let mut need_fallback_pass = true;
    if force_fast_compute {
      // iGPU-Leichtgewicht-Interpolator aktiv: das geladene SDK bleibt im Speicher bereit
      // und erhaelt beim naechsten Zurueckschalten automatisch ein sauberes History-Reset.
      self.pending_reset = true;
    } else {
      self.pending_reset = false;
      if !self.sdk_handle.is_null() && matches!(self.mode, Fsr3Mode::NativeSdk { .. }) {
        let rc = unsafe {
          ff_fsr3_dispatch(
            self.sdk_handle,
            Interface::as_raw(cmd_list) as *mut c_void,
            Interface::as_raw(curr_color) as *mut c_void,
            Interface::as_raw(depth_buffer) as *mut c_void,
            Interface::as_raw(motion_vectors) as *mut c_void,
            Interface::as_raw(&interp_tex) as *mut c_void,
            display_width,
            display_height,
            mv_w,
            mv_h,
            delta_time_ms.max(1.0_f32),
            current_frame_id,
            if is_reset { 1 } else { 0 },
          )
        };

        if rc >= 0 {
          need_fallback_pass = false;
        } else {
          self.destroy_sdk_context();
          self.mode = Fsr3Mode::ComputeFallback {
            reason: format!("FSR-3-SDK-Dispatch fehlgeschlagen (Code {rc})"),
          };
        }
      }
    }

    if need_fallback_pass {
      let effective_prev = if is_reset { curr_color } else { prev_color };
      self.record_compute_fallback(
        device,
        cmd_list,
        slot,
        effective_prev,
        curr_color,
        motion_vectors,
        depth_buffer,
        &interp_tex,
        display_width,
        display_height,
        mv_w,
        mv_h,
        hud_protection,
      );
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

      cmd_list.ResourceBarrier(&[
        Self::uav_barrier(&interp_tex),
        Self::transition_barrier(
          &interp_tex,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
          SHADER_READ_STATES,
        ),
      ]);
    }

    Ok(())
  }

  pub fn interpolated_resource(&self) -> Option<&ID3D12Resource> {
    self.interpolated_texture.as_ref()
  }

  pub fn mode(&self) -> &Fsr3Mode {
    &self.mode
  }

  pub fn mode_summary(&self) -> String {
    match &self.mode {
      Fsr3Mode::Inactive => String::from("Inaktiv"),
      Fsr3Mode::NativeSdk { version } => format!("Nativ ({version})"),
      Fsr3Mode::ComputeFallback { reason } => format!("DX12-Fallback ({reason})"),
    }
  }

  pub fn active_status_label(&self, force_fast_compute: bool) -> String {
    if force_fast_compute {
      return String::from("FSR3 iGPU-Turbo (CS)");
    }
    match &self.mode {
      Fsr3Mode::Inactive => String::from("Inaktiv"),
      Fsr3Mode::NativeSdk { version } => {
        let trimmed = version.trim();
        if trimmed.starts_with("FSR") || trimmed.starts_with("AMD") {
          trimmed.to_string()
        } else {
          format!("FSR {trimmed}")
        }
      }
      Fsr3Mode::ComputeFallback { .. } => String::from("Fallback (CS)"),
    }
  }

  pub fn is_using_native_sdk(&self, force_fast_compute: bool) -> bool {
    !force_fast_compute && matches!(self.mode, Fsr3Mode::NativeSdk { .. })
  }

  #[allow(dead_code)]
  pub fn allocated_dimensions(&self) -> (u32, u32, u32, u32) {
    (
      self.alloc_display_w,
      self.alloc_display_h,
      self.alloc_mv_w,
      self.alloc_mv_h,
    )
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
        ff_fsr3_destroy(self.sdk_handle);
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
  fn record_compute_fallback(
    &self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    slot: usize,
    prev_color: &ID3D12Resource,
    curr_color: &ID3D12Resource,
    motion_vectors: &ID3D12Resource,
    depth_buffer: &ID3D12Resource,
    out_interpolated: &ID3D12Resource,
    display_w: u32,
    display_h: u32,
    mv_w: u32,
    mv_h: u32,
    hud_protection: bool,
  ) {
    let base = slot * DESCRIPTORS_PER_SLOT;
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

    let constants = FallbackRootConstants {
      display_w: display_w.max(1),
      display_h: display_h.max(1),
      mv_w: mv_w.max(1),
      mv_h: mv_h.max(1),
      hud_protection_flag: if hud_protection { 1_u32 } else { 0_u32 },
      _pad0: 0,
      _pad1: 0,
      _pad2: 0,
    };

    unsafe {
      cmd_list.SetDescriptorHeaps(&[Some(self.fallback_heap.clone())]);
      cmd_list.SetComputeRootSignature(&self.fallback_root_sig);
      cmd_list.SetPipelineState(&self.fallback_pso);
      cmd_list.SetComputeRoot32BitConstants(
        0,
        FALLBACK_ROOT_CONSTANT_COUNT,
        &constants as *const FallbackRootConstants as *const c_void,
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
    let start = unsafe { self.fallback_heap.GetCPUDescriptorHandleForHeapStart() };
    D3D12_CPU_DESCRIPTOR_HANDLE {
      ptr: start.ptr + index * self.descriptor_size,
    }
  }

  fn gpu_descriptor_handle(&self, index: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
    let start = unsafe { self.fallback_heap.GetGPUDescriptorHandleForHeapStart() };
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
      -100 => String::from("Ohne FidelityFX-SDK-Header gebaut"),
      -2 => String::from("amd_fidelityfx_loader_dx12.dll nicht gefunden"),
      -4 => String::from("FidelityFX-API-Symbole unvollstaendig"),
      other => format!("FidelityFX-Init Code {other}"),
    }
  }

  fn create_interpolated_texture(
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
        .context("CreateCommittedResource fuer FSR-3-Zieltextur fehlgeschlagen")?;
    }
    texture.context("FSR-3-Zieltextur wurde nicht zurueckgegeben")
  }

  fn create_fallback_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
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
            Num32BitValues: FALLBACK_ROOT_CONSTANT_COUNT,
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
      bail!("Fsr3Interpolator RootSignature-Serialisierung fehlgeschlagen ({err}): {detail}");
    }

    let blob = sig_blob.context("Kein serialisierter RootSignature-Blob fuer Fsr3Interpolator")?;
    let bytes = unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    unsafe {
      device
        .CreateRootSignature(0, bytes)
        .context("CreateRootSignature fuer Fsr3Interpolator fehlgeschlagen")
    }
  }

  fn create_fallback_pso(
    device: &ID3D12Device,
    root_sig: &ID3D12RootSignature,
  ) -> Result<ID3D12PipelineState> {
    let cs_blob = Self::compile_fallback_cs()?;
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
    pso_res.context("CreateComputePipelineState fuer FSR-3-Fallback fehlgeschlagen")
  }

  fn compile_fallback_cs() -> Result<ID3DBlob> {
    let mut code_blob: Option<ID3DBlob> = None;
    let mut err_blob: Option<ID3DBlob> = None;
    let res = unsafe {
      D3DCompile(
        FALLBACK_INTERP_HLSL.as_ptr() as *const c_void,
        FALLBACK_INTERP_HLSL.len(),
        PCSTR(b"frameforge_fsr3_fallback_cs.hlsl\0".as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(b"cs_fsr3_fallback\0".as_ptr()),
        PCSTR(b"cs_5_0\0".as_ptr()),
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code_blob,
        Some(&mut err_blob),
      )
    };
    if let Err(err) = res {
      let detail = Self::blob_to_string(err_blob.as_ref());
      bail!("D3DCompile fuer cs_fsr3_fallback fehlgeschlagen ({err}): {detail}");
    }
    code_blob.context("Kein Bytecode-Blob fuer cs_fsr3_fallback erhalten")
  }

  fn try_create_gpu_timer(
    device: &ID3D12Device,
    cmd_queue: &ID3D12CommandQueue,
  ) -> Option<Fsr3GpuTimer> {
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

    Some(Fsr3GpuTimer {
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

impl Drop for Fsr3Interpolator {
  fn drop(&mut self) {
    self.destroy_sdk_context();
  }
}
