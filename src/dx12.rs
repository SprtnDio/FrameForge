use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::ptr::copy_nonoverlapping;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use egui::{ClippedPrimitive, TexturesDelta};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D::{
  D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_12_0, D3D_FEATURE_LEVEL_12_1,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
  DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN,
  DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
  CreateDXGIFactory2, IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory4, IDXGIFactory5, IDXGIFactory6,
  IDXGISwapChain3, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_DEBUG,
  DXGI_CREATE_FACTORY_FLAGS, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
  DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
  DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
  DXGI_PRESENT_ALLOW_TEARING, DXGI_QUERY_VIDEO_MEMORY_INFO, DXGI_SCALING_STRETCH,
  DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
  DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

use crate::backend::{FrameGenBackend, SyntheticGBufferInputs};
use crate::dlss45::{Dlss45Interpolator, Dlss45Mode};
use crate::fsr3::{Fsr3Interpolator, Fsr3Mode};
use crate::gbuffer::{GBufferParams, GBufferStats, SyntheticGBuffer, SHADER_READ_STATES};
use crate::ui::{DebugViewMode, LogStore, UpscalerBackend};
use crate::ui_renderer::UiRenderer;
use crate::xess3::{XeSS3Interpolator, XeSS3Mode};

pub const BUFFER_COUNT: usize = 3;
const STANDBY_CLEAR_COLOR: [f32; 4] = [0.035_f32, 0.055_f32, 0.11_f32, 1.0_f32];
const ACTIVE_VIDEO_CLEAR_COLOR: [f32; 4] = [0.0_f32, 0.0_f32, 0.0_f32, 1.0_f32];
const VENDOR_ID_NVIDIA: u32 = 0x10DE;
const VENDOR_ID_AMD: u32 = 0x1002;
const VENDOR_ID_INTEL: u32 = 0x8086;

const DEFAULT_CAPTURE_INTERVAL_MS: f32 = 16.67_f32;
const MIN_CAPTURE_INTERVAL_MS: f32 = 8.0_f32;
const MAX_CAPTURE_INTERVAL_MS: f32 = 50.0_f32;
const CAPTURE_DISCONTINUITY_RESET_MS: f32 = 250.0_f32;

struct VideoGpuResources {
  video_texture: ID3D12Resource,
  prev_video_texture: ID3D12Resource,
  upload_buffers: [Option<ID3D12Resource>; BUFFER_COUNT],
  width: u32,
  height: u32,
  aligned_row_pitch: usize,
  has_history_frame: bool,
}

/// Einzeln per Checkbox de-/aktivierbare Optionen des FrameForge Performance-Turbo-Pakets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerformanceTurboConfig {
  /// 50 % G-Buffer-Aufloesung (z. B. 960x540 statt 1920x1080) fuer ~75 % weniger Compute-Last.
  pub half_res_flow_scale: bool,
  /// Schneller logarithmischer 5-Tap-Diamond-Suchradius im G-Buffer statt Vollsuche.
  pub fast_gbuffer_search: bool,
  /// iGPU-Leichtgewicht-Interpolator (DX12 Compute) statt schwerer nativer KI-/7-Pass-SDK-Pfade.
  pub force_fast_compute_fg: bool,
  /// Automatischer HUD-, Minimap- & Untertitel-Schutz bei bewegtem Hintergrund.
  pub hud_protection: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoRenderParams {
  pub delta_time_ms: f32,
  pub expand_limited_rgb: bool,
  pub rcas_sharpness: f32,
  pub allow_tearing: bool,
  pub debug_view_mode: DebugViewMode,
  pub gbuffer: GBufferParams,
  pub upscaler_backend: UpscalerBackend,
  pub frame_generation_enabled: bool,
  pub fg_multiplier: u32,
  pub turbo: PerformanceTurboConfig,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VramUsageInfo {
  pub local_used_mb: u64,
  pub local_budget_mb: u64,
  pub shared_used_mb: u64,
  pub shared_budget_mb: u64,
  pub is_integrated_shared: bool,
}

pub struct Dx12Context {
  // Wichtig fuer Drop-Reihenfolge: `fsr3`, `xess3`, `dlss45`, `video_gpu`, `gbuffer` und
  // `ui_renderer` stehen vor `command_queue` und `device`, nachdem `Drop::drop` zuerst
  // `wait_for_gpu()` ausfuehrt.
  fsr3: Fsr3Interpolator,
  xess3: XeSS3Interpolator,
  dlss45: Dlss45Interpolator,
  video_gpu: Option<VideoGpuResources>,
  gbuffer: SyntheticGBuffer,
  pending_video_copy: bool,
  pending_unique_frame: bool,
  pending_upload_slot: usize,
  has_live_video_frame: bool,
  unique_frame_count: u64,
  last_unique_arrival_instant: Instant,
  last_unique_frame_instant: Instant,
  smoothed_capture_interval_ms: f32,
  fsr3_prepared_dims: Option<(u32, u32, u32, u32)>,
  xess3_prepared_dims: Option<(u32, u32, u32, u32)>,
  dlss45_prepared_dims: Option<(u32, u32)>,
  active_upscaler_backend: UpscalerBackend,
  fg_was_active: bool,
  active_effective_multiplier: u32,
  fg_force_reset_pending: bool,
  valid_generated_frames: usize,
  presenting_interpolated_frame: bool,
  last_gbuffer_params: Option<GBufferParams>,
  last_turbo: PerformanceTurboConfig,
  ui_renderer: UiRenderer,
  command_list: ID3D12GraphicsCommandList,
  command_allocator: ID3D12CommandAllocator,
  render_targets: Vec<ID3D12Resource>,
  rtv_heap: ID3D12DescriptorHeap,
  rtv_descriptor_size: usize,
  fence: ID3D12Fence,
  fence_value: u64,
  frame_fence_values: [u64; BUFFER_COUNT],
  fence_event: HANDLE,
  swapchain: IDXGISwapChain3,
  command_queue: ID3D12CommandQueue,
  device: ID3D12Device,
  adapter: IDXGIAdapter1,
  #[allow(dead_code)]
  factory: IDXGIFactory4,
  adapter_name: String,
  dedicated_vram_mb: u64,
  frame_index: usize,
  width: u32,
  height: u32,
  tearing_supported: bool,
}

impl Dx12Context {
  pub fn new(hwnd: HWND, width: u32, height: u32, logs: &mut LogStore) -> Result<Self> {
    let (width, height) = (width.max(1), height.max(1));
    let factory = Self::create_dxgi_factory(Self::enable_debug_layer_if_available(logs))?;
    let tearing_supported = Self::check_tearing_support(&factory);
    let (adapter, adapter_name, device) = Self::select_adapter_and_create_device(&factory, logs)?;
    let dedicated_vram_mb = unsafe { adapter.GetDesc1() }
      .map(|d| (d.DedicatedVideoMemory as u64) / (1024 * 1024))
      .unwrap_or(0);
    logs.info(format!(
      "[FrameForge][DX12] Aktiver Adapter: {} (Dedicated VRAM: {} MB) | AllowTearing: {}",
      adapter_name, dedicated_vram_mb, tearing_supported
    ));

    let qdesc = D3D12_COMMAND_QUEUE_DESC {
      Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
      Priority: 0,
      Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
      NodeMask: 0,
    };
    let command_queue: ID3D12CommandQueue = unsafe {
      device
        .CreateCommandQueue(&qdesc)
        .context("CreateCommandQueue fehlgeschlagen")?
    };

    let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
      Width: width,
      Height: height,
      Format: DXGI_FORMAT_R8G8B8A8_UNORM,
      Stereo: BOOL(0),
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
      BufferCount: BUFFER_COUNT as u32,
      Scaling: DXGI_SCALING_STRETCH,
      SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
      AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
      Flags: if tearing_supported {
        DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32
      } else {
        0
      },
    };
    let swap1 = unsafe {
      factory
        .CreateSwapChainForHwnd(&command_queue, hwnd, &swap_desc, None, None)
        .context("CreateSwapChainForHwnd fehlgeschlagen")?
    };
    unsafe {
      factory
        .MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)
        .context("MakeWindowAssociation fehlgeschlagen")?;
    }
    let swapchain: IDXGISwapChain3 = swap1.cast().context("Cast auf IDXGISwapChain3 fehlgeschlagen")?;
    let frame_index = (unsafe { swapchain.GetCurrentBackBufferIndex() } as usize) % BUFFER_COUNT;
    logs.info(format!(
      "[FrameForge][DX12] SwapChain bereit: {}x{} px ({}x R8G8B8A8_UNORM, Flip-Discard)",
      width, height, BUFFER_COUNT
    ));

    let rtv_desc = D3D12_DESCRIPTOR_HEAP_DESC {
      Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
      NumDescriptors: BUFFER_COUNT as u32,
      Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
      NodeMask: 0,
    };
    let rtv_heap: ID3D12DescriptorHeap = unsafe {
      device
        .CreateDescriptorHeap(&rtv_desc)
        .context("RTV-DescriptorHeap fehlgeschlagen")?
    };
    let rtv_descriptor_size = unsafe {
      device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV)
    } as usize;
    let render_targets = Self::create_rtvs(&device, &swapchain, &rtv_heap, rtv_descriptor_size)?;

    let command_allocator: ID3D12CommandAllocator = unsafe {
      device
        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        .context("CreateCommandAllocator fehlgeschlagen")?
    };
    let command_list: ID3D12GraphicsCommandList = unsafe {
      device
        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &command_allocator, None)
        .context("CreateCommandList fehlgeschlagen")?
    };
    unsafe {
      command_list
        .Close()
        .context("Initiales CommandList::Close fehlgeschlagen")?;
    }

    let fence: ID3D12Fence = unsafe {
      device
        .CreateFence(0, D3D12_FENCE_FLAG_NONE)
        .context("CreateFence fehlgeschlagen")?
    };
    let fence_event = unsafe {
      CreateEventW(None, false, false, None).context("CreateEventW fehlgeschlagen")?
    };

    let gbuffer = SyntheticGBuffer::new(&device, &command_queue, logs)?;
    let fsr3 = Fsr3Interpolator::new(&device, &command_queue)
      .context("Initialisierung des Fsr3Interpolator fehlgeschlagen")?;
    let xess3 = XeSS3Interpolator::new(&device, &command_queue)
      .context("Initialisierung des XeSS3Interpolator fehlgeschlagen")?;
    let dlss45 = Dlss45Interpolator::new(&device, &command_queue)
      .context("Initialisierung des Dlss45Interpolator fehlgeschlagen")?;
    logs.info(
      "[FrameForge][FG] Multi-Vendor-Pipelines bereit: AMD FSR (SDK 2.4 / 4.x+3.1), Intel XeSS 3.0.2 & Nvidia DLSS 4.5 (2x/3x/4x MFG).",
    );

    let ui_renderer = UiRenderer::new(&device)?;
    logs.info("[FrameForge][DX12] Eigener egui-DX12-Renderer (HLSL vs_5_0 / ps_5_0) initialisiert.");

    let init_instant = Instant::now();
    let mut ctx = Self {
      fsr3,
      xess3,
      dlss45,
      video_gpu: None,
      gbuffer,
      pending_video_copy: false,
      pending_unique_frame: false,
      pending_upload_slot: frame_index,
      has_live_video_frame: false,
      unique_frame_count: 0,
      last_unique_arrival_instant: init_instant,
      last_unique_frame_instant: init_instant,
      smoothed_capture_interval_ms: DEFAULT_CAPTURE_INTERVAL_MS,
      fsr3_prepared_dims: None,
      xess3_prepared_dims: None,
      dlss45_prepared_dims: None,
      active_upscaler_backend: UpscalerBackend::Passthrough,
      fg_was_active: false,
      active_effective_multiplier: 1,
      fg_force_reset_pending: true,
      valid_generated_frames: 0,
      presenting_interpolated_frame: false,
      last_gbuffer_params: None,
      last_turbo: PerformanceTurboConfig::default(),
      ui_renderer,
      command_list,
      command_allocator,
      render_targets,
      rtv_heap,
      rtv_descriptor_size,
      fence,
      fence_value: 0,
      frame_fence_values: [0; BUFFER_COUNT],
      fence_event,
      swapchain,
      command_queue,
      device,
      adapter,
      factory,
      adapter_name,
      dedicated_vram_mb,
      frame_index,
      width,
      height,
      tearing_supported,
    };
    ctx.wait_for_gpu()?;
    Ok(ctx)
  }

  pub fn upload_capture_frame(
    &mut self,
    width: u32,
    height: u32,
    rgba_bytes: &[u8],
    is_duplicate: bool,
    logs: &mut LogStore,
  ) -> Result<()> {
    let row_bytes = (width as usize) * 4;
    let expected_len = row_bytes.saturating_mul(height as usize);
    if width == 0 || height == 0 || rgba_bytes.len() < expected_len {
      return Ok(());
    }

    if self
      .video_gpu
      .as_ref()
      .map(|g| g.width != width || g.height != height)
      .unwrap_or(true)
    {
      // GPU vollstaendig synchronisieren, BEVOR Ressourcen ausserhalb einer offenen CommandList neu angelegt werden!
      self.wait_for_gpu()?;
      self.video_gpu = None;
      self.last_gbuffer_params = None;
      self.unique_frame_count = 0;
      self.smoothed_capture_interval_ms = DEFAULT_CAPTURE_INTERVAL_MS;
      self.fsr3_prepared_dims = None;
      self.xess3_prepared_dims = None;
      self.dlss45_prepared_dims = None;
      self.valid_generated_frames = 0;
      self.presenting_interpolated_frame = false;
      self.fg_force_reset_pending = true;

      let align = D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize;
      let aligned_row_pitch = (row_bytes + align - 1) & !(align - 1);
      let video_texture = Self::create_default_tex2d(
        &self.device,
        width,
        height,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        SHADER_READ_STATES,
      )?;
      let prev_video_texture = Self::create_default_tex2d(
        &self.device,
        width,
        height,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        SHADER_READ_STATES,
      )?;
      let upload_byte_size = (aligned_row_pitch * (height as usize)) as u64;
      let upload_buffers = [
        Some(Self::create_upload_buffer(&self.device, upload_byte_size)?),
        Some(Self::create_upload_buffer(&self.device, upload_byte_size)?),
        Some(Self::create_upload_buffer(&self.device, upload_byte_size)?),
      ];

      let half_res = self.last_turbo.half_res_flow_scale;
      let (mv_w, mv_h) = SyntheticGBuffer::compute_target_dimensions(width, height, half_res);

      self.gbuffer.ensure_resources(
        &self.device,
        width,
        height,
        half_res,
        &video_texture,
        &prev_video_texture,
        Some(logs),
      )?;

      logs.info(format!(
        "[FrameForge][DX12] Temporale Video-Historie (Frame N + Frame N-1) + {BUFFER_COUNT}x Upload-Ring: {}x{} px (RowPitch: {} B).",
        width, height, aligned_row_pitch
      ));

      // VRAM-schonendes Lazy-Init: nur das aktuell ausgewaehlte Frame-Gen-Backend allokiert Texturen/SDK!
      let active_backend = self.active_upscaler_backend;
      let fast_fg = self.last_turbo.force_fast_compute_fg;
      self.ensure_selected_backend_resources(
        active_backend,
        width,
        height,
        mv_w,
        mv_h,
        fast_fg,
        Some(logs),
      )?;

      self.video_gpu = Some(VideoGpuResources {
        video_texture,
        prev_video_texture,
        upload_buffers,
        width,
        height,
        aligned_row_pitch,
        has_history_frame: false,
      });
    }

    let write_slot = (unsafe { self.swapchain.GetCurrentBackBufferIndex() } as usize) % BUFFER_COUNT;
    self.wait_for_fence_value(self.frame_fence_values[write_slot])?;

    let Some(gpu) = self.video_gpu.as_ref() else {
      return Ok(());
    };
    let Some(upload_buf) = gpu.upload_buffers[write_slot].as_ref() else {
      return Ok(());
    };

    unsafe {
      let mut ptr: *mut c_void = std::ptr::null_mut();
      upload_buf
        .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut ptr))
        .context("Map auf Upload-Ring-Buffer fehlgeschlagen")?;
      let dst = ptr as *mut u8;
      if gpu.aligned_row_pitch == row_bytes {
        copy_nonoverlapping(rgba_bytes.as_ptr(), dst, expected_len);
      } else {
        for r in 0..(height as usize) {
          copy_nonoverlapping(
            rgba_bytes.as_ptr().add(r * row_bytes),
            dst.add(r * gpu.aligned_row_pitch),
            row_bytes,
          );
        }
      }
      upload_buf.Unmap(0, None);
    }

    let had_history = gpu.has_history_frame;
    self.pending_upload_slot = write_slot;
    self.pending_video_copy = true;
    if !is_duplicate || !had_history {
      self.pending_unique_frame = true;
    }
    self.has_live_video_frame = true;
    Ok(())
  }

  pub fn clear_video_signal(&mut self) {
    self.pending_video_copy = false;
    self.pending_unique_frame = false;
    self.has_live_video_frame = false;
    self.unique_frame_count = 0;
    self.smoothed_capture_interval_ms = DEFAULT_CAPTURE_INTERVAL_MS;
    self.valid_generated_frames = 0;
    self.presenting_interpolated_frame = false;
    self.fg_was_active = false;
    self.active_effective_multiplier = 1;
    self.fg_force_reset_pending = true;
    self.last_gbuffer_params = None;
    if let Some(gpu) = &mut self.video_gpu {
      gpu.has_history_frame = false;
    }
    self.gbuffer.reset_signal();
    self.fsr3.request_reset();
    self.xess3.request_reset();
    self.dlss45.request_reset();
  }

  pub fn gbuffer_stats(&self) -> GBufferStats {
    self
      .gbuffer
      .stats(self.has_live_video_frame && self.video_gpu.is_some())
  }

  pub fn gbuffer_dimensions(&self) -> (u32, u32) {
    self.gbuffer.dimensions()
  }

  pub fn gbuffer_gpu_time_ms(&self) -> f32 {
    if self.has_live_video_frame && self.video_gpu.is_some() {
      self.gbuffer.last_gpu_time_ms()
    } else {
      0.0_f32
    }
  }

  pub fn fg_gpu_time_ms(&self) -> f32 {
    if !self.fg_was_active || !self.has_live_video_frame || self.video_gpu.is_none() {
      return 0.0_f32;
    }
    match self.active_upscaler_backend {
      UpscalerBackend::Passthrough => 0.0_f32,
      UpscalerBackend::AmdFsr3 => self.fsr3.last_gpu_time_ms(),
      UpscalerBackend::IntelXeSSFg => self.xess3.last_gpu_time_ms(),
      UpscalerBackend::NvidiaDlssFg => self.dlss45.last_gpu_time_ms(),
    }
  }

  pub fn active_fg_status_label(&self) -> String {
    let fast = self.last_turbo.force_fast_compute_fg;
    match self.active_upscaler_backend {
      UpscalerBackend::Passthrough => String::from("Inaktiv"),
      UpscalerBackend::AmdFsr3 => self.fsr3.active_status_label(fast),
      UpscalerBackend::IntelXeSSFg => self.xess3.active_status_label(fast),
      UpscalerBackend::NvidiaDlssFg => self.dlss45.active_status_label(fast),
    }
  }

  pub fn active_fg_is_native_sdk(&self) -> bool {
    let fast = self.last_turbo.force_fast_compute_fg;
    match self.active_upscaler_backend {
      UpscalerBackend::Passthrough => false,
      UpscalerBackend::AmdFsr3 => self.fsr3.is_using_native_sdk(fast),
      UpscalerBackend::IntelXeSSFg => self.xess3.is_using_native_sdk(fast),
      UpscalerBackend::NvidiaDlssFg => self.dlss45.is_using_native_sdk(fast),
    }
  }

  pub fn active_fg_multiplier(&self) -> u32 {
    if self.fg_was_active {
      self.active_effective_multiplier.clamp(2, 4)
    } else {
      1_u32
    }
  }

  pub fn is_presenting_interpolated_frame(&self) -> bool {
    self.presenting_interpolated_frame
  }

  pub fn render_frame(
    &mut self,
    backend: &mut dyn FrameGenBackend,
    vp: VideoRenderParams,
    ui_prims: &[ClippedPrimitive],
    ui_delta: &TexturesDelta,
    ppp: f32,
  ) -> Result<()> {
    if self.width == 0 || self.height == 0 {
      return Ok(());
    }

    let now = Instant::now();
    let turbo = vp.turbo;
    let flow_scale_changed = turbo.half_res_flow_scale != self.last_turbo.half_res_flow_scale;
    let turbo_changed = turbo != self.last_turbo;
    self.last_turbo = turbo;

    let requested_mult = vp.fg_multiplier.clamp(2, 4);
    let effective_mult = if !vp.frame_generation_enabled {
      1_u32
    } else {
      match vp.upscaler_backend {
        UpscalerBackend::Passthrough => 1_u32,
        UpscalerBackend::AmdFsr3 => 2_u32,
        UpscalerBackend::IntelXeSSFg | UpscalerBackend::NvidiaDlssFg => requested_mult,
      }
    };
    let fg_active = effective_mult >= 2;

    let backend_changed = vp.upscaler_backend != self.active_upscaler_backend;
    let mult_changed = effective_mult != self.active_effective_multiplier;
    let just_activated_fg = fg_active && !self.fg_was_active;

    if backend_changed || mult_changed || turbo_changed || just_activated_fg {
      self.request_backend_reset(vp.upscaler_backend);
      self.fg_force_reset_pending = true;
    }
    if backend_changed || mult_changed || flow_scale_changed || !fg_active {
      self.valid_generated_frames = 0;
      if !fg_active {
        self.presenting_interpolated_frame = false;
      }
    }

    self.active_upscaler_backend = vp.upscaler_backend;
    self.fg_was_active = fg_active;
    self.active_effective_multiplier = effective_mult;

    // Wenn ein Frame-Gen-Backend aktiv ist, liefert der Synthetic G-Buffer zwingend aktuelle Motion-Vektoren & Depth.
    let mut effective_gbuffer = vp.gbuffer;
    effective_gbuffer.half_res_flow_scale = turbo.half_res_flow_scale;
    effective_gbuffer.fast_search = turbo.fast_gbuffer_search;
    if fg_active {
      effective_gbuffer.enabled = true;
    }

    // GPU-sicheres & VRAM-schonendes Live-Umschalten VOR CommandAllocator::Reset() und CommandList::Reset():
    // Wenn sich Capture-Aufloesung, 50%-Flow-Scale oder das ausgewaehlte Backend geaendert hat,
    // synchronisiere zuerst die GPU via wait_for_gpu() und initialisiere gezielt NUR das aktive Backend!
    if let Some((cap_w, cap_h, curr_tex, prev_tex)) = self.video_gpu.as_ref().map(|g| {
      (
        g.width,
        g.height,
        g.video_texture.clone(),
        g.prev_video_texture.clone(),
      )
    }) {
      let (mv_w, mv_h) =
        SyntheticGBuffer::compute_target_dimensions(cap_w, cap_h, turbo.half_res_flow_scale);

      let gb_needs_recreate = effective_gbuffer.enabled
        && (flow_scale_changed
          || self
            .gbuffer
            .needs_resize(cap_w, cap_h, turbo.half_res_flow_scale));

      let active_backend_needs_prepare = match vp.upscaler_backend {
        UpscalerBackend::Passthrough => false,
        UpscalerBackend::AmdFsr3 => {
          backend_changed
            || flow_scale_changed
            || self.fsr3_prepared_dims != Some((cap_w, cap_h, mv_w, mv_h))
            || matches!(self.fsr3.mode(), Fsr3Mode::Inactive)
        }
        UpscalerBackend::IntelXeSSFg => {
          backend_changed
            || flow_scale_changed
            || self.xess3_prepared_dims != Some((cap_w, cap_h, mv_w, mv_h))
            || matches!(self.xess3.mode(), XeSS3Mode::Inactive)
        }
        UpscalerBackend::NvidiaDlssFg => {
          backend_changed
            || self.dlss45_prepared_dims != Some((cap_w, cap_h))
            || matches!(self.dlss45.mode(), Dlss45Mode::Inactive)
        }
      };

      if gb_needs_recreate || active_backend_needs_prepare || flow_scale_changed || backend_changed {
        self.wait_for_gpu()?;

        if effective_gbuffer.enabled || flow_scale_changed {
          self.gbuffer.ensure_resources(
            &self.device,
            cap_w,
            cap_h,
            turbo.half_res_flow_scale,
            &curr_tex,
            &prev_tex,
            None,
          )?;
          self.last_gbuffer_params = None;
        }

        if active_backend_needs_prepare {
          self.ensure_selected_backend_resources(
            vp.upscaler_backend,
            cap_w,
            cap_h,
            mv_w,
            mv_h,
            turbo.force_fast_compute_fg,
            None,
          )?;
          self.fg_force_reset_pending = true;
        }
      }
    }

    self.frame_index = (unsafe { self.swapchain.GetCurrentBackBufferIndex() } as usize) % BUFFER_COUNT;
    let slot = self.frame_index;
    self.wait_for_fence_value(self.frame_fence_values[slot])?;

    let back_buffer = self.render_targets[slot].clone();
    let rtv_handle = self.rtv_handle(slot);

    unsafe {
      self
        .command_allocator
        .Reset()
        .context("CommandAllocator::Reset fehlgeschlagen")?;
      self
        .command_list
        .Reset(&self.command_allocator, None)
        .context("CommandList::Reset fehlgeschlagen")?;
    }
    self.gbuffer.set_enabled_state(effective_gbuffer.enabled);

    let mut did_unique_upload = false;
    let mut capture_dt_ms = self.smoothed_capture_interval_ms;
    let mut capture_gap_reset = false;

    if self.pending_video_copy {
      let is_unique = self.pending_unique_frame;
      let upload_slot = self.pending_upload_slot % BUFFER_COUNT;
      if let Some(gpu) = &mut self.video_gpu {
        if !gpu.has_history_frame {
          Self::record_upload_to_tex(&self.command_list, gpu, upload_slot);
          Self::record_copy_tex(&self.command_list, &gpu.prev_video_texture, &gpu.video_texture);
          gpu.has_history_frame = true;
          did_unique_upload = true;
          self.unique_frame_count = 1;
          self.last_unique_arrival_instant = now;
          self.last_unique_frame_instant = now;
          self.valid_generated_frames = 0;
        } else if is_unique {
          // Kadenzsicher: Frame N -> Frame N-1 sichern, bevor das neue echte Frame N aus dem Upload-Ring kopiert wird!
          Self::record_copy_tex(&self.command_list, &gpu.prev_video_texture, &gpu.video_texture);
          Self::record_upload_to_tex(&self.command_list, gpu, upload_slot);
          did_unique_upload = true;
          self.unique_frame_count = self.unique_frame_count.saturating_add(1);

          let raw_interval_ms =
            now.duration_since(self.last_unique_arrival_instant).as_secs_f32() * 1000.0_f32;
          self.last_unique_arrival_instant = now;

          if raw_interval_ms > CAPTURE_DISCONTINUITY_RESET_MS {
            capture_gap_reset = true;
            capture_dt_ms = self.smoothed_capture_interval_ms;
          } else {
            capture_dt_ms = raw_interval_ms.clamp(MIN_CAPTURE_INTERVAL_MS, MAX_CAPTURE_INTERVAL_MS);
            self.smoothed_capture_interval_ms = (self.smoothed_capture_interval_ms * 0.80_f32
              + capture_dt_ms * 0.20_f32)
              .clamp(MIN_CAPTURE_INTERVAL_MS, MAX_CAPTURE_INTERVAL_MS);
          }

          if !fg_active {
            self.last_unique_frame_instant = now;
          }
        } else {
          // 60->30 Duplikat: Frame N-1 UND `last_unique_frame_instant` bleiben unangetastet!
          Self::record_upload_to_tex(&self.command_list, gpu, upload_slot);
        }
      }
      self.pending_video_copy = false;
      self.pending_unique_frame = false;
    }

    if self.has_live_video_frame && self.video_gpu.is_some() {
      let need_gbuffer_dispatch =
        did_unique_upload || self.last_gbuffer_params != Some(effective_gbuffer);
      if need_gbuffer_dispatch {
        if let Some((cap_w, cap_h, curr_tex, prev_tex)) = self.video_gpu.as_ref().map(|g| {
          (
            g.width,
            g.height,
            g.video_texture.clone(),
            g.prev_video_texture.clone(),
          )
        }) {
          self.gbuffer.dispatch(
            &self.device,
            &self.command_list,
            slot,
            &curr_tex,
            &prev_tex,
            cap_w,
            cap_h,
            effective_gbuffer,
            turbo.half_res_flow_scale,
            turbo.fast_gbuffer_search,
          )?;
          self.last_gbuffer_params = Some(effective_gbuffer);
        }
      }

      if did_unique_upload && self.unique_frame_count >= 2 && fg_active {
        if let Some(gpu) = self.video_gpu.as_ref() {
          let (fallback_mv_w, fallback_mv_h) = SyntheticGBuffer::compute_target_dimensions(
            gpu.width,
            gpu.height,
            turbo.half_res_flow_scale,
          );
          let (gb_w, gb_h) = self.gbuffer.dimensions();
          let mv_w = if gb_w > 0 { gb_w } else { fallback_mv_w };
          let mv_h = if gb_h > 0 { gb_h } else { fallback_mv_h };

          let mv_res = self.gbuffer.motion_vectors_resource();
          let depth_res = self.gbuffer.depth_buffer_resource();
          let force_reset =
            just_activated_fg || self.fg_force_reset_pending || capture_gap_reset;

          match vp.upscaler_backend {
            UpscalerBackend::Passthrough => {
              self.valid_generated_frames = 0;
            }
            UpscalerBackend::AmdFsr3 => {
              self.fsr3.dispatch(
                &self.device,
                &self.command_list,
                slot,
                &gpu.prev_video_texture,
                &gpu.video_texture,
                &mv_res,
                &depth_res,
                gpu.width,
                gpu.height,
                mv_w,
                mv_h,
                capture_dt_ms,
                force_reset,
                turbo.force_fast_compute_fg,
                turbo.hud_protection,
              )?;
              self.valid_generated_frames = usize::from(self.fsr3.interpolated_resource().is_some());
            }
            UpscalerBackend::IntelXeSSFg => {
              self.xess3.dispatch(
                &self.device,
                &self.command_list,
                slot,
                &gpu.prev_video_texture,
                &gpu.video_texture,
                &mv_res,
                &depth_res,
                gpu.width,
                gpu.height,
                mv_w,
                mv_h,
                effective_mult,
                force_reset,
                turbo.force_fast_compute_fg,
                turbo.hud_protection,
              )?;
              let expected = (effective_mult - 1) as usize;
              self.valid_generated_frames = (0..expected)
                .take_while(|&idx| self.xess3.interpolated_resource(idx).is_some())
                .count();
            }
            UpscalerBackend::NvidiaDlssFg => {
              self.dlss45.dispatch(
                &self.device,
                &self.command_list,
                slot,
                &gpu.prev_video_texture,
                &gpu.video_texture,
                &mv_res,
                &depth_res,
                gpu.width,
                gpu.height,
                mv_w,
                mv_h,
                effective_mult,
                force_reset,
                turbo.force_fast_compute_fg,
                turbo.hud_protection,
              )?;
              let expected = (effective_mult - 1) as usize;
              self.valid_generated_frames = (0..expected)
                .take_while(|&idx| self.dlss45.interpolated_resource(idx).is_some())
                .count();
            }
          }

          if self.valid_generated_frames > 0 {
            // Pacing-Anker NUR aktualisieren, wenn ein echtes Nicht-Duplikat-Frame hochgeladen
            // UND erfolgreich neu interpoliert wurde (verhindert jedes Zurueckspringen auf Phase 0
            // oder Aufblitzen alter N-0.5-Zwischenbilder nach Frame N).
            self.last_unique_frame_instant = now;
          }
          self.fg_force_reset_pending = false;
        }
      }
    }

    let clear = if self.has_live_video_frame && self.video_gpu.is_some() {
      ACTIVE_VIDEO_CLEAR_COLOR
    } else {
      STANDBY_CLEAR_COLOR
    };
    unsafe {
      self.command_list.ResourceBarrier(&[Self::transition(
        &back_buffer,
        D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
      )]);
      self.command_list.OMSetRenderTargets(
        1,
        Some(&rtv_handle as *const D3D12_CPU_DESCRIPTOR_HANDLE),
        false,
        None,
      );
      self
        .command_list
        .ClearRenderTargetView(rtv_handle, &clear, None);
    }

    self.presenting_interpolated_frame = false;
    if self.has_live_video_frame {
      if let Some(gpu) = &self.video_gpu {
        // Verallgemeinertes monotones 2x / 3x / 4x Multi-Frame-Pacing:
        // Bei effektivem Multiplikator M in {2, 3, 4} existieren M - 1 zeitlich geordnete
        // Zwischenbilder (Slot 0 .. M - 2) vor dem echten Capture-Bild Frame N (Phase M - 1).
        // Bei aktivem Intel XeSS 3 nutzt auch Phase M - 1 `reconstructed_curr_resource`, damit
        // Zwischenbilder und Echtbild Frame N identisches KI-AA ohne 30/60-Hz-Schaerfe-Pumpen zeigen.
        let m = effective_mult as usize;
        let mut presenting_interpolated = false;

        let real_frame_tex: &ID3D12Resource =
          if fg_active && vp.upscaler_backend == UpscalerBackend::IntelXeSSFg {
            self
              .xess3
              .reconstructed_curr_resource(turbo.force_fast_compute_fg)
              .unwrap_or(&gpu.video_texture)
          } else {
            &gpu.video_texture
          };

        let color_input = if fg_active && m >= 2 && self.valid_generated_frames >= (m - 1) {
          let elapsed_ms =
            now.duration_since(self.last_unique_frame_instant).as_secs_f32() * 1000.0_f32;
          let interval_ms = self.smoothed_capture_interval_ms.max(MIN_CAPTURE_INTERVAL_MS);
          let phase_idx = (((elapsed_ms / interval_ms).max(0.0_f32) * (m as f32)).floor() as usize)
            .min(m - 1);

          if phase_idx < (m - 1) {
            let maybe_interp_tex = match vp.upscaler_backend {
              UpscalerBackend::Passthrough => None,
              UpscalerBackend::AmdFsr3 => {
                if phase_idx == 0 {
                  self.fsr3.interpolated_resource()
                } else {
                  None
                }
              }
              UpscalerBackend::IntelXeSSFg => self.xess3.interpolated_resource(phase_idx),
              UpscalerBackend::NvidiaDlssFg => self.dlss45.interpolated_resource(phase_idx),
            };

            if let Some(interp_tex) = maybe_interp_tex {
              presenting_interpolated = true;
              interp_tex.clone()
            } else {
              real_frame_tex.clone()
            }
          } else {
            real_frame_tex.clone()
          }
        } else {
          real_frame_tex.clone()
        };
        self.presenting_interpolated_frame = presenting_interpolated;

        let inputs = SyntheticGBufferInputs {
          color_input,
          motion_vectors: self.gbuffer.motion_vectors_resource(),
          depth_buffer: self.gbuffer.depth_buffer_resource(),
          real_curr_color: Some(&gpu.video_texture),
          prev_color: Some(&gpu.prev_video_texture),
          gbuffer_size: self.gbuffer.dimensions(),
          is_interpolated_frame: presenting_interpolated,
          hud_protection: turbo.hud_protection,
          color_width: gpu.width,
          color_height: gpu.height,
          output_width: self.width,
          output_height: self.height,
          delta_time_ms: vp.delta_time_ms.max(0.0_f32),
          jitter_offset: [0.0_f32, 0.0_f32],
          expand_limited_rgb: vp.expand_limited_rgb,
          rcas_sharpness: vp.rcas_sharpness,
          debug_view_mode: vp.debug_view_mode,
        };
        // PassthroughBackend und UiRenderer binden nach jedem Compute-/SDK-Dispatch jeweils
        // ihre eigenen Descriptor-Heaps, PipelineStates und RootSignatures.
        backend.dispatch(&self.device, &self.command_list, &inputs)?;
      }
    }

    self.ui_renderer.render(
      &self.device,
      &self.command_list,
      ui_delta,
      ui_prims,
      self.width,
      self.height,
      ppp,
    )?;

    let allow_tearing_now =
      vp.allow_tearing && self.tearing_supported && !self.is_exclusive_fullscreen();

    unsafe {
      self.command_list.ResourceBarrier(&[Self::transition(
        &back_buffer,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_PRESENT,
      )]);
      self
        .command_list
        .Close()
        .context("CommandList::Close fehlgeschlagen")?;
      let base: ID3D12CommandList = self
        .command_list
        .cast()
        .context("Cast auf ID3D12CommandList fehlgeschlagen")?;
      self.command_queue.ExecuteCommandLists(&[Some(base)]);
      let (sync, flags) = if allow_tearing_now {
        (0, DXGI_PRESENT_ALLOW_TEARING)
      } else {
        (1, DXGI_PRESENT(0))
      };
      self
        .swapchain
        .Present(sync, flags)
        .ok()
        .context("SwapChain::Present fehlgeschlagen")?;
    }
    drop(back_buffer);
    self.wait_for_gpu()?;
    self.frame_fence_values[slot] = self.fence_value;
    self.gbuffer.read_completed_gpu_timestamp(slot);
    Ok(())
  }

  fn is_exclusive_fullscreen(&self) -> bool {
    let mut fullscreen = BOOL(0);
    unsafe {
      self
        .swapchain
        .GetFullscreenState(Some(&mut fullscreen), None)
        .is_ok()
        && fullscreen.as_bool()
    }
  }

  fn request_backend_reset(&mut self, backend: UpscalerBackend) {
    match backend {
      UpscalerBackend::Passthrough => {}
      UpscalerBackend::AmdFsr3 => self.fsr3.request_reset(),
      UpscalerBackend::IntelXeSSFg => self.xess3.request_reset(),
      UpscalerBackend::NvidiaDlssFg => self.dlss45.request_reset(),
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn ensure_selected_backend_resources(
    &mut self,
    backend: UpscalerBackend,
    cap_w: u32,
    cap_h: u32,
    mv_w: u32,
    mv_h: u32,
    force_fast_compute: bool,
    mut logs: Option<&mut LogStore>,
  ) -> Result<()> {
    match backend {
      UpscalerBackend::Passthrough => {}
      UpscalerBackend::AmdFsr3 => {
        self.fsr3.ensure_resources(
          &self.device,
          cap_w,
          cap_h,
          mv_w,
          mv_h,
          force_fast_compute,
        )?;
        self.fsr3_prepared_dims = Some((cap_w, cap_h, mv_w, mv_h));
        if let Some(l) = logs.as_deref_mut() {
          l.info(format!(
            "[FrameForge][FSR] Kontext fuer Display {}x{} (G-Buffer {}x{}) bereit: {}.",
            cap_w,
            cap_h,
            mv_w,
            mv_h,
            self.fsr3.mode_summary()
          ));
        }
      }
      UpscalerBackend::IntelXeSSFg => {
        self.xess3.ensure_resources(
          &self.device,
          cap_w,
          cap_h,
          mv_w,
          mv_h,
          force_fast_compute,
        )?;
        self.xess3_prepared_dims = Some((cap_w, cap_h, mv_w, mv_h));
        if let Some(l) = logs.as_deref_mut() {
          l.info(format!(
            "[FrameForge][XeSS3] Kontext fuer Display {}x{} (G-Buffer {}x{}) bereit: {}.",
            cap_w,
            cap_h,
            mv_w,
            mv_h,
            self.xess3.mode_summary()
          ));
        }
      }
      UpscalerBackend::NvidiaDlssFg => {
        self.dlss45.ensure_resources(
          &self.device,
          cap_w,
          cap_h,
          force_fast_compute,
        )?;
        self.dlss45_prepared_dims = Some((cap_w, cap_h));
        if let Some(l) = logs.as_deref_mut() {
          l.info(format!(
            "[FrameForge][DLSS4.5] Kontext fuer Display {}x{} bereit: {}.",
            cap_w,
            cap_h,
            self.dlss45.mode_summary()
          ));
        }
      }
    }
    Ok(())
  }

  fn record_upload_to_tex(
    cmd: &ID3D12GraphicsCommandList,
    gpu: &VideoGpuResources,
    upload_slot: usize,
  ) {
    let Some(upload_buf) = gpu.upload_buffers[upload_slot % BUFFER_COUNT].as_ref() else {
      return;
    };
    unsafe {
      let src_loc = D3D12_TEXTURE_COPY_LOCATION {
        pResource: std::mem::transmute_copy(upload_buf),
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
          PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
              Format: DXGI_FORMAT_R8G8B8A8_UNORM,
              Width: gpu.width,
              Height: gpu.height,
              Depth: 1,
              RowPitch: gpu.aligned_row_pitch as u32,
            },
          },
        },
      };
      let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
        pResource: std::mem::transmute_copy(&gpu.video_texture),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
          SubresourceIndex: 0,
        },
      };
      cmd.ResourceBarrier(&[Self::transition(
        &gpu.video_texture,
        SHADER_READ_STATES,
        D3D12_RESOURCE_STATE_COPY_DEST,
      )]);
      cmd.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
      cmd.ResourceBarrier(&[Self::transition(
        &gpu.video_texture,
        D3D12_RESOURCE_STATE_COPY_DEST,
        SHADER_READ_STATES,
      )]);
    }
  }

  fn record_copy_tex(cmd: &ID3D12GraphicsCommandList, dst: &ID3D12Resource, src: &ID3D12Resource) {
    unsafe {
      cmd.ResourceBarrier(&[
        Self::transition(src, SHADER_READ_STATES, D3D12_RESOURCE_STATE_COPY_SOURCE),
        Self::transition(dst, SHADER_READ_STATES, D3D12_RESOURCE_STATE_COPY_DEST),
      ]);
      cmd.CopyResource(dst, src);
      cmd.ResourceBarrier(&[
        Self::transition(src, D3D12_RESOURCE_STATE_COPY_SOURCE, SHADER_READ_STATES),
        Self::transition(dst, D3D12_RESOURCE_STATE_COPY_DEST, SHADER_READ_STATES),
      ]);
    }
  }

  pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || (width == self.width && height == self.height) {
      return Ok(());
    }
    self.wait_for_gpu()?;
    self.frame_fence_values = [self.fence_value; BUFFER_COUNT];
    self.render_targets.clear();
    let flags = if self.tearing_supported {
      DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
    } else {
      DXGI_SWAP_CHAIN_FLAG(0)
    };
    unsafe {
      self
        .swapchain
        .ResizeBuffers(
          BUFFER_COUNT as u32,
          width,
          height,
          DXGI_FORMAT_R8G8B8A8_UNORM,
          flags,
        )
        .context("ResizeBuffers fehlgeschlagen")?;
    }
    self.render_targets = Self::create_rtvs(
      &self.device,
      &self.swapchain,
      &self.rtv_heap,
      self.rtv_descriptor_size,
    )?;
    self.frame_index = (unsafe { self.swapchain.GetCurrentBackBufferIndex() } as usize) % BUFFER_COUNT;
    (self.width, self.height) = (width, height);
    Ok(())
  }

  fn wait_for_fence_value(&self, target: u64) -> Result<()> {
    if target == 0 {
      return Ok(());
    }
    unsafe {
      if self.fence.GetCompletedValue() < target {
        self
          .fence
          .SetEventOnCompletion(target, self.fence_event)
          .context("SetEventOnCompletion fehlgeschlagen")?;
        WaitForSingleObject(self.fence_event, INFINITE);
      }
    }
    Ok(())
  }

  pub fn wait_for_gpu(&mut self) -> Result<()> {
    self.fence_value = self.fence_value.wrapping_add(1);
    let target = self.fence_value;
    unsafe {
      self
        .command_queue
        .Signal(&self.fence, target)
        .context("CommandQueue::Signal fehlgeschlagen")?;
    }
    self.wait_for_fence_value(target)
  }

  pub fn device(&self) -> &ID3D12Device {
    &self.device
  }

  pub fn swapchain(&self) -> &IDXGISwapChain3 {
    &self.swapchain
  }

  pub fn adapter_name(&self) -> &str {
    &self.adapter_name
  }

  pub fn tearing_supported(&self) -> bool {
    self.tearing_supported
  }

  pub fn dimensions(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  pub fn query_vram_info(&self) -> VramUsageInfo {
    let Ok(a3) = self.adapter.cast::<IDXGIAdapter3>() else {
      return VramUsageInfo {
        is_integrated_shared: self.dedicated_vram_mb < 512,
        ..Default::default()
      };
    };
    let (mut loc, mut sh) = (
      DXGI_QUERY_VIDEO_MEMORY_INFO::default(),
      DXGI_QUERY_VIDEO_MEMORY_INFO::default(),
    );
    let loc_ok = unsafe {
      a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut loc)
        .is_ok()
    };
    let sh_ok = unsafe {
      a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut sh)
        .is_ok()
    };
    let (lu, lb) = if loc_ok {
      (
        loc.CurrentUsage / (1024 * 1024),
        loc.Budget / (1024 * 1024),
      )
    } else {
      (0, 0)
    };
    let (su, sb) = if sh_ok {
      (
        sh.CurrentUsage / (1024 * 1024),
        sh.Budget / (1024 * 1024),
      )
    } else {
      (0, 0)
    };
    let is_integrated_shared =
      self.dedicated_vram_mb < 512 || (lu == 0 && su > 0) || (lb == 0 && sb > 0);
    VramUsageInfo {
      local_used_mb: lu,
      local_budget_mb: lb,
      shared_used_mb: su,
      shared_budget_mb: sb,
      is_integrated_shared,
    }
  }

  fn create_default_tex2d(
    device: &ID3D12Device,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    state: D3D12_RESOURCE_STATES,
  ) -> Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_DEFAULT,
      ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
      Alignment: 0,
      Width: w.max(1) as u64,
      Height: h.max(1),
      DepthOrArraySize: 1,
      MipLevels: 1,
      Format: fmt,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
      Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let mut tex: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut tex)
        .context("CreateCommittedResource (Texture2D) fehlgeschlagen")?;
    }
    tex.context("Keine Texture2D erhalten")
  }

  fn create_upload_buffer(device: &ID3D12Device, byte_size: u64) -> Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_UPLOAD,
      ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
      Alignment: 0,
      Width: byte_size.max(256),
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
    let mut buf: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(
          &props,
          D3D12_HEAP_FLAG_NONE,
          &desc,
          D3D12_RESOURCE_STATE_GENERIC_READ,
          None,
          &mut buf,
        )
        .context("CreateCommittedResource (UPLOAD) fehlgeschlagen")?;
    }
    buf.context("Kein Upload-Buffer erhalten")
  }

  fn enable_debug_layer_if_available(logs: &mut LogStore) -> bool {
    #[cfg(debug_assertions)]
    unsafe {
      let mut dbg: Option<ID3D12Debug> = None;
      if D3D12GetDebugInterface(&mut dbg).is_ok() {
        if let Some(d) = dbg {
          d.EnableDebugLayer();
          logs.info("[FrameForge][DX12] D3D12-Debug-Layer aktiviert.");
          return true;
        }
      } else {
        logs.warn("[FrameForge][DX12] D3D12-Debug-Layer nicht verfuegbar.");
      }
    }
    #[cfg(not(debug_assertions))]
    logs.info("[FrameForge][DX12] Release-Build aktiv (ohne Debug-Layer-Overhead).");
    false
  }

  fn create_dxgi_factory(enable_debug: bool) -> Result<IDXGIFactory4> {
    if enable_debug {
      if let Ok(f) = unsafe { CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_DEBUG) } {
        return Ok(f);
      }
    }
    unsafe {
      CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
        .context("CreateDXGIFactory2 fehlgeschlagen")
    }
  }

  fn check_tearing_support(factory: &IDXGIFactory4) -> bool {
    let Ok(f5) = factory.cast::<IDXGIFactory5>() else {
      return false;
    };
    let mut allow = BOOL(0);
    unsafe {
      f5.CheckFeatureSupport(
        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
        &mut allow as *mut BOOL as *mut c_void,
        size_of::<BOOL>() as u32,
      )
      .is_ok()
        && allow.as_bool()
    }
  }

  fn select_adapter_and_create_device(
    factory: &IDXGIFactory4,
    logs: &mut LogStore,
  ) -> Result<(IDXGIAdapter1, String, ID3D12Device)> {
    let levels = [
      D3D_FEATURE_LEVEL_12_1,
      D3D_FEATURE_LEVEL_12_0,
      D3D_FEATURE_LEVEL_11_0,
    ];
    let mut candidates: Vec<(u64, IDXGIAdapter1, String)> = Vec::new();
    let mut push_cand = |a: IDXGIAdapter1, rank: u32| {
      if let Ok(d) = unsafe { a.GetDesc1() } {
        if (d.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) == 0 {
          candidates.push((
            Self::score_hardware_adapter(&d, rank),
            a,
            Self::decode_adapter_name(&d),
          ));
        }
      }
    };
    if let Ok(f6) = factory.cast::<IDXGIFactory6>() {
      let mut idx = 0_u32;
      while let Ok(a) = unsafe {
        f6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(idx, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
      } {
        push_cand(a, idx);
        idx += 1;
      }
    } else {
      let mut idx = 0_u32;
      while let Ok(a) = unsafe { factory.EnumAdapters1(idx) } {
        push_cand(a, idx);
        idx += 1;
      }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_score, adapter, name) in candidates {
      if let Some(dev) = Self::try_create_device(&adapter, &levels) {
        return Ok((adapter, name, dev));
      }
    }
    logs.warn("[FrameForge][DX12] Keine Hardware-GPU gefunden - Notfall-Fallback auf WARP.");
    let warp: IDXGIAdapter1 = unsafe {
      factory
        .EnumWarpAdapter()
        .context("WARP nicht verfuegbar")?
    };
    let warp_name = unsafe { warp.GetDesc1() }
      .map(|d| Self::decode_adapter_name(&d))
      .unwrap_or_else(|_| String::from("Microsoft Basic Render Driver (WARP)"));
    if let Some(dev) = Self::try_create_device(&warp, &levels) {
      return Ok((warp, warp_name, dev));
    }
    bail!("Konnte kein ID3D12Device erstellen")
  }

  fn try_create_device(
    adapter: &IDXGIAdapter1,
    levels: &[D3D_FEATURE_LEVEL],
  ) -> Option<ID3D12Device> {
    for &lvl in levels {
      let mut dev: Option<ID3D12Device> = None;
      if unsafe { D3D12CreateDevice(adapter, lvl, &mut dev) }.is_ok() && dev.is_some() {
        return dev;
      }
    }
    None
  }

  fn score_hardware_adapter(desc: &DXGI_ADAPTER_DESC1, preference_rank: u32) -> u64 {
    let vram_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
    let vendor_bonus = if matches!(
      desc.VendorId,
      VENDOR_ID_NVIDIA | VENDOR_ID_AMD | VENDOR_ID_INTEL
    ) {
      100_000
    } else {
      0
    };
    vendor_bonus + 10_000_u64.saturating_sub((preference_rank as u64) * 500) + vram_mb
  }

  fn decode_adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let nul = desc
      .Description
      .iter()
      .position(|&c| c == 0)
      .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..nul])
  }

  fn create_rtvs(
    device: &ID3D12Device,
    swapchain: &IDXGISwapChain3,
    heap: &ID3D12DescriptorHeap,
    stride: usize,
  ) -> Result<Vec<ID3D12Resource>> {
    let base = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
    let mut out = Vec::with_capacity(BUFFER_COUNT);
    for i in 0..BUFFER_COUNT {
      let buf: ID3D12Resource = unsafe {
        swapchain
          .GetBuffer(i as u32)
          .with_context(|| format!("GetBuffer({i}) fehlgeschlagen"))?
      };
      unsafe {
        device.CreateRenderTargetView(
          &buf,
          None,
          D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + i * stride,
          },
        );
      }
      out.push(buf);
    }
    Ok(out)
  }

  fn rtv_handle(&self, idx: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    D3D12_CPU_DESCRIPTOR_HANDLE {
      ptr: unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() }.ptr
        + idx * self.rtv_descriptor_size,
    }
  }

  fn transition(
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
}

impl Drop for Dx12Context {
  fn drop(&mut self) {
    let _ = self.wait_for_gpu();
    unsafe {
      if !self.fence_event.is_invalid() {
        let _ = CloseHandle(self.fence_event);
      }
    }
  }
}
