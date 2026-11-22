use std::collections::VecDeque;
use std::mem::size_of;
use std::time::{Duration, Instant};

use egui::{
  pos2, Area, Color32, Context, CursorIcon, FontId, Frame, Id, Margin, Order, Pos2, Rect,
  Rounding, Sense, Shape, Stroke, TextWrapMode, Ui, Vec2,
};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetSystemTimes};

use crate::audio::AudioLiveTelemetry;
use crate::dx12::{PerformanceTurboConfig, VramUsageInfo};
use crate::gbuffer::GBufferStats;
use crate::i18n::{tr, AppLanguage};
use crate::ui::{DebugViewMode, UpscalerBackend};

const MIN_HW_POLL_MS: u32 = 500;
const FT_CAP: usize = 1200;
const GRAPH_CAP: usize = 150;

pub const OSD_REFRESH_INTERVALS_MS: [u32; 4] = [250, 500, 1000, 2000];

pub fn osd_refresh_interval_label(ms: u32, lang: AppLanguage) -> &'static str {
  match ms {
    250 => "250 ms",
    500 => tr(lang, "500 ms (Empfohlen)", "500 ms (Recommended)"),
    1000 => "1000 ms",
    2000 => "2000 ms",
    _ => tr(lang, "500 ms (Empfohlen)", "500 ms (Recommended)"),
  }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
  Color32::from_rgb(r, g, b)
}

const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
  let a32 = a as u32;
  let rp = ((r as u32 * a32 + 127) / 255) as u8;
  let gp = ((g as u32 * a32 + 127) / 255) as u8;
  let bp = ((b as u32 * a32 + 127) / 255) as u8;
  Color32::from_rgba_premultiplied(rp, gp, bp, a)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdPreset {
  CompactOneLine,
  StandardOverlay,
  FullDiagnostics,
  Custom,
}

impl OsdPreset {
  pub const ALL: [Self; 4] = [
    Self::CompactOneLine,
    Self::StandardOverlay,
    Self::FullDiagnostics,
    Self::Custom,
  ];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::CompactOneLine => tr(lang, "Kompakt (1 Zeile)", "Compact (1 Line)"),
      Self::StandardOverlay => tr(lang, "Standard-Overlay", "Standard Overlay"),
      Self::FullDiagnostics => tr(lang, "Volle Diagnose (Alles)", "Full Diagnostics (All)"),
      Self::Custom => tr(lang, "Benutzerdefiniert", "Custom"),
    }
  }

  pub fn cycle_next(self) -> Self {
    match self {
      Self::CompactOneLine => Self::StandardOverlay,
      Self::StandardOverlay => Self::FullDiagnostics,
      Self::FullDiagnostics => Self::Custom,
      Self::Custom => Self::CompactOneLine,
    }
  }

  #[allow(dead_code)]
  pub fn config_key(self) -> &'static str {
    match self {
      Self::CompactOneLine => "compact",
      Self::StandardOverlay => "standard",
      Self::FullDiagnostics => "full",
      Self::Custom => "custom",
    }
  }

  #[allow(dead_code)]
  pub fn from_config_key(key: &str) -> Self {
    match key.trim().to_ascii_lowercase().as_str() {
      "compact" | "one_line" => Self::CompactOneLine,
      "full" | "diagnostics" => Self::FullDiagnostics,
      "custom" => Self::Custom,
      _ => Self::StandardOverlay,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdQuickSlot {
  TopLeft,
  TopCenter,
  TopRight,
  BottomLeft,
  BottomRight,
}

impl OsdQuickSlot {
  pub const ALL: [Self; 5] = [
    Self::TopLeft,
    Self::TopCenter,
    Self::TopRight,
    Self::BottomLeft,
    Self::BottomRight,
  ];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::TopLeft => tr(lang, "Oben Links", "Top Left"),
      Self::TopCenter => tr(lang, "Oben Mitte", "Top Center"),
      Self::TopRight => tr(lang, "Oben Rechts", "Top Right"),
      Self::BottomLeft => tr(lang, "Unten Links", "Bottom Left"),
      Self::BottomRight => tr(lang, "Unten Rechts", "Bottom Right"),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdColorTheme {
  AmberClassic,
  CyanTech,
  Emerald,
  MonochromeWhite,
}

impl OsdColorTheme {
  pub const ALL: [Self; 4] = [
    Self::AmberClassic,
    Self::CyanTech,
    Self::Emerald,
    Self::MonochromeWhite,
  ];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::AmberClassic => "Amber Classic",
      Self::CyanTech => "Cyan Tech",
      Self::Emerald => "Emerald",
      Self::MonochromeWhite => tr(lang, "Monochrom Weiss", "Monochrome White"),
    }
  }

  #[allow(dead_code)]
  pub fn config_key(self) -> &'static str {
    match self {
      Self::AmberClassic => "amber",
      Self::CyanTech => "cyan",
      Self::Emerald => "emerald",
      Self::MonochromeWhite => "mono",
    }
  }

  #[allow(dead_code)]
  pub fn from_config_key(key: &str) -> Self {
    match key.trim().to_ascii_lowercase().as_str() {
      "cyan" => Self::CyanTech,
      "emerald" | "green" => Self::Emerald,
      "mono" | "white" | "monochrome" => Self::MonochromeWhite,
      _ => Self::AmberClassic,
    }
  }

  pub fn accent_color(self) -> Color32 {
    match self {
      Self::AmberClassic => rgb(245, 158, 11),
      Self::CyanTech => rgb(56, 189, 248),
      Self::Emerald => rgb(52, 211, 153),
      Self::MonochromeWhite => rgb(203, 213, 225),
    }
  }

  pub fn highlight_color(self) -> Color32 {
    match self {
      Self::AmberClassic => rgb(252, 211, 77),
      Self::CyanTech => rgb(125, 211, 252),
      Self::Emerald => rgb(110, 231, 183),
      Self::MonochromeWhite => Color32::WHITE,
    }
  }

  pub fn value_color(self) -> Color32 {
    match self {
      Self::Emerald => rgb(220, 252, 231),
      _ => rgb(245, 247, 250),
    }
  }

  pub fn unit_color(self) -> Color32 {
    match self {
      Self::AmberClassic => rgb(253, 186, 116),
      Self::CyanTech => rgb(147, 197, 253),
      Self::Emerald => rgb(134, 239, 172),
      Self::MonochromeWhite => rgb(180, 190, 205),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsdRowFlags {
  pub gpu_and_vram: bool,
  pub cpu_usage: bool,
  pub system_and_app_ram: bool,
  pub capture_signal: bool,
  pub gbuffer_compute: bool,
  pub pipeline_backend: bool,
  pub audio_engine: bool,
  pub output_fps_frametime: bool,
  pub fps_percentiles: bool,
  pub frametime_graph: bool,
}

impl OsdRowFlags {
  pub fn for_preset(preset: OsdPreset) -> Self {
    match preset {
      OsdPreset::CompactOneLine => Self {
        gpu_and_vram: false,
        cpu_usage: false,
        system_and_app_ram: false,
        capture_signal: true,
        gbuffer_compute: true,
        pipeline_backend: true,
        audio_engine: true,
        output_fps_frametime: true,
        fps_percentiles: true,
        frametime_graph: false,
      },
      OsdPreset::StandardOverlay => Self {
        gpu_and_vram: true,
        cpu_usage: true,
        system_and_app_ram: true,
        capture_signal: true,
        gbuffer_compute: true,
        pipeline_backend: true,
        audio_engine: false,
        output_fps_frametime: true,
        fps_percentiles: true,
        frametime_graph: true,
      },
      OsdPreset::FullDiagnostics | OsdPreset::Custom => Self {
        gpu_and_vram: true,
        cpu_usage: true,
        system_and_app_ram: true,
        capture_signal: true,
        gbuffer_compute: true,
        pipeline_backend: true,
        audio_engine: true,
        output_fps_frametime: true,
        fps_percentiles: true,
        frametime_graph: true,
      },
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsdConfig {
  pub enabled: bool,
  pub locked: bool,
  pub hide_in_menu: bool,
  pub pos_norm: [f32; 2],
  pub last_size_pts: [f32; 2],
  pub preset: OsdPreset,
  pub refresh_interval_ms: u32,
  pub box_width: f32,
  pub scale_percent: f32,
  pub background_opacity_percent: f32,
  pub color_theme: OsdColorTheme,
  pub rows: OsdRowFlags,
}

impl Default for OsdConfig {
  fn default() -> Self {
    let preset = OsdPreset::StandardOverlay;
    Self {
      enabled: true,
      locked: false,
      hide_in_menu: false,
      pos_norm: [0.015_f32, 0.075_f32],
      last_size_pts: [450.0_f32, 205.0_f32],
      preset,
      refresh_interval_ms: 500,
      box_width: 430.0_f32,
      scale_percent: 100.0_f32,
      background_opacity_percent: 48.0_f32,
      color_theme: OsdColorTheme::AmberClassic,
      rows: OsdRowFlags::for_preset(preset),
    }
  }
}

impl OsdConfig {
  pub fn apply_preset(&mut self, preset: OsdPreset) {
    self.preset = preset;
    if preset != OsdPreset::Custom {
      self.rows = OsdRowFlags::for_preset(preset);
    }
  }

  pub fn cycle_preset(&mut self) -> OsdPreset {
    let next = self.preset.cycle_next();
    self.apply_preset(next);
    next
  }

  pub fn fixed_inner_width(&self) -> f32 {
    let scale = (self.scale_percent / 100.0_f32).clamp(0.75_f32, 1.60_f32);
    let base_w = self.box_width.clamp(320.0_f32, 620.0_f32);
    if self.preset == OsdPreset::CompactOneLine {
      (base_w * 1.35_f32) * scale
    } else {
      base_w * scale
    }
  }

  pub fn fixed_outer_width(&self) -> f32 {
    let scale = (self.scale_percent / 100.0_f32).clamp(0.75_f32, 1.60_f32);
    let op = (self.background_opacity_percent / 100.0_f32).clamp(0.0_f32, 0.90_f32);
    let bg_a = (op * 255.0_f32).round() as u8;
    let pad_x = if bg_a == 0 { 2.0_f32 } else { 10.0_f32 * scale };
    self.fixed_inner_width() + 2.0_f32 * pad_x
  }

  pub fn apply_quick_slot(&mut self, slot: OsdQuickSlot, screen_size: Vec2) {
    let (sw, sh) = (screen_size.x.max(320.0_f32), screen_size.y.max(240.0_f32));
    let (ow, oh) = (
      self.fixed_outer_width().clamp(80.0_f32, sw - 24.0_f32),
      self.last_size_pts[1].clamp(24.0_f32, sh - 24.0_f32),
    );
    let pad = 16.0_f32;
    let top_y = if self.hide_in_menu { pad } else { 58.0_f32 };
    let bottom_pad = if self.hide_in_menu { pad } else { 38.0_f32 };
    let bot_y = (sh - oh - bottom_pad).max(pad);
    let (px, py) = match slot {
      OsdQuickSlot::TopLeft => (pad, top_y),
      OsdQuickSlot::TopCenter => (((sw - ow) * 0.5_f32).max(pad), top_y),
      OsdQuickSlot::TopRight => ((sw - ow - pad).max(pad), top_y),
      OsdQuickSlot::BottomLeft => (pad, bot_y),
      OsdQuickSlot::BottomRight => ((sw - ow - pad).max(pad), bot_y),
    };
    self.pos_norm = [
      (px / sw).clamp(0.0_f32, 1.0_f32),
      (py / sh).clamp(0.0_f32, 1.0_f32),
    ];
  }
}

/// Rohwerte eines einzelnen Render-Frames fuer die interne Telemetrie-Akkumulation.
#[derive(Clone, Copy, Debug)]
pub struct FrameTelemetrySample {
  pub has_signal: bool,
  pub capture_raw_fps: f32,
  pub capture_unique_fps: f32,
  pub capture_dup_pct: f32,
  pub gbuffer_gpu_ms: f32,
  pub fg_gpu_ms: f32,
  pub active_fg_multiplier: u32,
  pub effective_video_fps: f32,
  pub audio_latency_ms: f32,
}

/// Eingefrorener, ruhig ablesbarer Anzeige-Snapshot, der sich nur im gewaehlten Intervall aktualisiert.
#[derive(Clone, Copy, Debug)]
pub struct LatchedSnapshot {
  pub avg_d3d12_fps: f32,
  pub avg_frametime_ms: f32,
  pub low_1_fps: f32,
  pub low_01_fps: f32,
  pub gbuf_gpu_ms: f32,
  pub fg_gpu_ms: f32,
  pub in_raw_fps: f32,
  pub in_unique_fps: f32,
  pub duplicate_ratio_pct: f32,
  pub effective_video_fps: f32,
  pub audio_latency_ms: f32,
  pub ft_min_ms: f32,
  pub ft_max_ms: f32,
}

impl Default for LatchedSnapshot {
  fn default() -> Self {
    Self {
      avg_d3d12_fps: 60.0_f32,
      avg_frametime_ms: 16.67_f32,
      low_1_fps: 0.0_f32,
      low_01_fps: 0.0_f32,
      gbuf_gpu_ms: 0.0_f32,
      fg_gpu_ms: 0.0_f32,
      in_raw_fps: 0.0_f32,
      in_unique_fps: 0.0_f32,
      duplicate_ratio_pct: 0.0_f32,
      effective_video_fps: 0.0_f32,
      audio_latency_ms: 0.0_f32,
      ft_min_ms: 0.0_f32,
      ft_max_ms: 0.0_f32,
    }
  }
}

#[derive(Clone, Debug)]
pub struct OsdRuntimeContext<'a> {
  pub adapter_name: &'a str,
  pub swapchain_size: (u32, u32),
  pub allow_tearing_active: bool,
  pub capture_has_signal: bool,
  pub capture_resolution: Option<(u32, u32)>,
  pub capture_raw_fps: f32,
  pub capture_unique_fps: f32,
  pub capture_duplicate_ratio_pct: f32,
  pub capture_format_label: &'a str,
  pub expand_limited_rgb: bool,
  pub duplicate_filter_active: bool,
  pub gbuffer: GBufferStats,
  pub gbuffer_dims: (u32, u32),
  pub gbuffer_gpu_ms: f32,
  pub debug_view_mode: DebugViewMode,
  pub backend: UpscalerBackend,
  pub frame_gen_enabled: bool,
  pub fg_multiplier: u32,
  pub fg_gpu_ms: f32,
  pub fg_status_label: String,
  pub fg_native_sdk: bool,
  pub effective_video_fps: f32,
  pub is_interpolated_phase: bool,
  pub cas_sharpness: f32,
  pub turbo: PerformanceTurboConfig,
  pub audio: AudioLiveTelemetry,
  pub menu_visible: bool,
}

pub struct SystemTelemetry {
  last_frame: Instant,
  live_dt_ms: f32,
  ft_win: VecDeque<f32>,
  scratch: Vec<f32>,
  // Interne Akkumulatoren fuer den gedrosselten LatchedSnapshot
  last_snapshot_instant: Instant,
  snapshot_initialized: bool,
  last_had_signal: bool,
  last_multiplier: u32,
  sum_dt_secs: f64,
  frame_count: u32,
  sum_gbuf_ms: f64,
  sum_fg_ms: f64,
  sum_in_raw_fps: f64,
  sum_in_unique_fps: f64,
  sum_dup_pct: f64,
  sum_effective_video_fps: f64,
  sum_audio_latency_ms: f64,
  latched: LatchedSnapshot,
  // Zeit-Bucket-Glaettung fuer ruhigen Frametime-Graphen ohne 600-Hz-Saegezahn
  last_bucket_instant: Instant,
  bucket_elapsed_ms: f64,
  bucket_frames: u32,
  bucket_ema_ms: f32,
  bucket_ema_init: bool,
  graph_ms: VecDeque<f32>,
  graph_top_ms_smoothed: f32,
  // System-Sensoren (CPU / RAM / VRAM)
  last_hw: Instant,
  hw_init: bool,
  prev_ticks: (u64, u64, u64),
  cpu_pct: f32,
  ram_used_gb: f32,
  ram_total_gb: f32,
  proc_ram_mb: f32,
  vram: VramUsageInfo,
  fg_phase_window: u32,
  fg_interpolated_frames: u64,
}

impl Default for SystemTelemetry {
  fn default() -> Self {
    Self::new()
  }
}

impl SystemTelemetry {
  pub fn new() -> Self {
    let now = Instant::now();
    let mut s = Self {
      last_frame: now,
      live_dt_ms: 16.67_f32,
      ft_win: VecDeque::with_capacity(FT_CAP),
      scratch: Vec::with_capacity(FT_CAP),
      last_snapshot_instant: now,
      snapshot_initialized: false,
      last_had_signal: false,
      last_multiplier: 1,
      sum_dt_secs: 0.0_f64,
      frame_count: 0,
      sum_gbuf_ms: 0.0_f64,
      sum_fg_ms: 0.0_f64,
      sum_in_raw_fps: 0.0_f64,
      sum_in_unique_fps: 0.0_f64,
      sum_dup_pct: 0.0_f64,
      sum_effective_video_fps: 0.0_f64,
      sum_audio_latency_ms: 0.0_f64,
      latched: LatchedSnapshot::default(),
      last_bucket_instant: now,
      bucket_elapsed_ms: 0.0_f64,
      bucket_frames: 0,
      bucket_ema_ms: 16.67_f32,
      bucket_ema_init: false,
      graph_ms: VecDeque::with_capacity(GRAPH_CAP),
      graph_top_ms_smoothed: 18.0_f32,
      last_hw: now,
      hw_init: false,
      prev_ticks: sample_cpu_ticks().unwrap_or((0, 0, 0)),
      cpu_pct: 0.0_f32,
      ram_used_gb: 0.0_f32,
      ram_total_gb: 0.0_f32,
      proc_ram_mb: 0.0_f32,
      vram: VramUsageInfo::default(),
      fg_phase_window: 0,
      fg_interpolated_frames: 0,
    };
    s.poll_mem();
    s
  }

  /// Akkumuliert pro Render-Frame intern alle Messwerte und aktualisiert sowohl den
  /// zeitbasierten Frametime-Bucket als auch den angezeigten `LatchedSnapshot` ausschliesslich
  /// nach Ablauf der jeweiligen Zeitfenster.
  pub fn record_frame_and_poll(
    &mut self,
    sample: FrameTelemetrySample,
    refresh_interval_ms: u32,
    query_vram: impl FnOnce() -> VramUsageInfo,
  ) {
    let now = Instant::now();
    let dt_secs = now
      .saturating_duration_since(self.last_frame)
      .as_secs_f64()
      .clamp(0.00005_f64, 0.5_f64);
    let dt_ms = (dt_secs * 1000.0_f64) as f32;
    self.last_frame = now;
    self.live_dt_ms = dt_ms;

    // 1. Ringpuffer fuer echte 1% / 0.1% Lows fuellen (keine UI-Ausgabe in diesem Schritt)
    if self.ft_win.len() >= FT_CAP {
      self.ft_win.pop_front();
    }
    self.ft_win.push_back(dt_ms);

    // 2. Zeitbasierter Frametime-Bucket gegen 600-Hz-Saegezahn-Flimmern
    let clamped_refresh_ms = refresh_interval_ms.clamp(250, 2000);
    let bucket_interval_ms = (clamped_refresh_ms / 5).max(80);
    self.bucket_elapsed_ms += dt_secs * 1000.0_f64;
    self.bucket_frames = self.bucket_frames.saturating_add(1);

    if now.saturating_duration_since(self.last_bucket_instant)
      >= Duration::from_millis(bucket_interval_ms as u64)
    {
      let avg_bucket_ms = ((self.bucket_elapsed_ms / (self.bucket_frames.max(1) as f64)) as f32)
        .clamp(0.05_f32, 200.0_f32);
      let smoothed_pt = if !self.bucket_ema_init {
        self.bucket_ema_init = true;
        avg_bucket_ms
      } else {
        self.bucket_ema_ms * 0.65_f32 + avg_bucket_ms * 0.35_f32
      };
      self.bucket_ema_ms = smoothed_pt;
      if self.graph_ms.len() >= GRAPH_CAP {
        self.graph_ms.pop_front();
      }
      self.graph_ms.push_back(smoothed_pt);
      self.bucket_elapsed_ms = 0.0_f64;
      self.bucket_frames = 0;
      self.last_bucket_instant = now;

      // Sanfte, zeitbasierte Nachfuehrung der oberen Y-Achsen-Skalierung des Graphen
      let mut max_g = smoothed_pt;
      for &v in &self.graph_ms {
        max_g = max_g.max(v);
      }
      let target_top = (max_g * 1.25_f32).clamp(6.0_f32, 66.7_f32);
      if target_top > self.graph_top_ms_smoothed {
        self.graph_top_ms_smoothed =
          self.graph_top_ms_smoothed * 0.78_f32 + target_top * 0.22_f32;
      } else {
        self.graph_top_ms_smoothed =
          self.graph_top_ms_smoothed * 0.94_f32 + target_top * 0.06_f32;
      }
    }

    // 3. Interne Frame-Akkumulatoren fuer den beruhigten LatchedSnapshot
    self.sum_dt_secs += dt_secs;
    self.frame_count = self.frame_count.saturating_add(1);
    self.sum_gbuf_ms += sample.gbuffer_gpu_ms.max(0.0_f32) as f64;
    self.sum_fg_ms += sample.fg_gpu_ms.max(0.0_f32) as f64;
    self.sum_in_raw_fps += sample.capture_raw_fps.max(0.0_f32) as f64;
    self.sum_in_unique_fps += sample.capture_unique_fps.max(0.0_f32) as f64;
    self.sum_dup_pct += sample.capture_dup_pct.clamp(0.0_f32, 100.0_f32) as f64;
    self.sum_effective_video_fps += sample.effective_video_fps.max(0.0_f32) as f64;
    self.sum_audio_latency_ms += sample.audio_latency_ms.max(0.0_f32) as f64;

    // Sofortige Reaktion bei Signalverlust oder direktem Wechsel des Multi-FG-Faktors (2x/3x/4x)
    if !sample.has_signal && self.last_had_signal {
      self.latched.in_raw_fps = 0.0_f32;
      self.latched.in_unique_fps = 0.0_f32;
      self.latched.duplicate_ratio_pct = 0.0_f32;
      self.latched.effective_video_fps = 0.0_f32;
    } else if sample.has_signal && sample.active_fg_multiplier != self.last_multiplier {
      self.latched.effective_video_fps =
        self.latched.in_unique_fps * (sample.active_fg_multiplier.max(1) as f32);
    }
    self.last_had_signal = sample.has_signal;
    self.last_multiplier = sample.active_fg_multiplier;

    // 4. LatchedSnapshot NUR nach Ablauf von `refresh_interval_ms` neu einfrieren
    let snap_elapsed = now.saturating_duration_since(self.last_snapshot_instant);
    if !self.snapshot_initialized
      || snap_elapsed >= Duration::from_millis(clamped_refresh_ms as u64)
    {
      let n = self.frame_count.max(1) as f64;
      let avg_d3d12_fps = if self.snapshot_initialized {
        let secs = snap_elapsed.as_secs_f64().max(self.sum_dt_secs).max(1e-4_f64);
        ((self.frame_count.max(1) as f64) / secs) as f32
      } else {
        (1000.0_f32 / dt_ms).clamp(1.0_f32, 9999.0_f32)
      };
      let avg_frametime_ms = if avg_d3d12_fps > 0.001_f32 {
        1000.0_f32 / avg_d3d12_fps
      } else {
        0.0_f32
      };
      let (low_1_fps, low_01_fps) = self.compute_lows();
      let (ft_min_ms, ft_max_ms) = if self.graph_ms.is_empty() {
        (avg_frametime_ms, avg_frametime_ms)
      } else {
        let mut mn = f32::MAX;
        let mut mx = 0.0_f32;
        for &v in &self.graph_ms {
          mn = mn.min(v);
          mx = mx.max(v);
        }
        (if mn.is_finite() { mn } else { 0.0_f32 }, mx)
      };

      self.latched = LatchedSnapshot {
        avg_d3d12_fps,
        avg_frametime_ms,
        low_1_fps,
        low_01_fps,
        gbuf_gpu_ms: (self.sum_gbuf_ms / n) as f32,
        fg_gpu_ms: (self.sum_fg_ms / n) as f32,
        in_raw_fps: if sample.has_signal {
          (self.sum_in_raw_fps / n) as f32
        } else {
          0.0_f32
        },
        in_unique_fps: if sample.has_signal {
          (self.sum_in_unique_fps / n) as f32
        } else {
          0.0_f32
        },
        duplicate_ratio_pct: if sample.has_signal {
          (self.sum_dup_pct / n) as f32
        } else {
          0.0_f32
        },
        effective_video_fps: if sample.has_signal {
          (self.sum_effective_video_fps / n) as f32
        } else {
          0.0_f32
        },
        audio_latency_ms: (self.sum_audio_latency_ms / n) as f32,
        ft_min_ms,
        ft_max_ms,
      };

      self.sum_dt_secs = 0.0_f64;
      self.frame_count = 0;
      self.sum_gbuf_ms = 0.0_f64;
      self.sum_fg_ms = 0.0_f64;
      self.sum_in_raw_fps = 0.0_f64;
      self.sum_in_unique_fps = 0.0_f64;
      self.sum_dup_pct = 0.0_f64;
      self.sum_effective_video_fps = 0.0_f64;
      self.sum_audio_latency_ms = 0.0_f64;
      self.last_snapshot_instant = now;
      self.snapshot_initialized = true;
    }

    // 5. Hardware-Sensoren (CPU / RAM / VRAM) mindestens auf MIN_HW_POLL_MS bzw. Snapshot-Takt gedrosselt
    let hw_poll_ms = clamped_refresh_ms.max(MIN_HW_POLL_MS);
    if !self.hw_init
      || now.saturating_duration_since(self.last_hw) >= Duration::from_millis(hw_poll_ms as u64)
    {
      self.last_hw = now;
      self.hw_init = true;
      self.poll_cpu();
      self.poll_mem();
      self.vram = query_vram();
    }
  }

  /// Verarbeitet `is_interpolated_phase` intern in einem gleitenden 32-Frame-Fenster + Zaehler,
  /// damit das OSD fuer 2x / 3x / 4x Multi-FG eine 100 % laengenstabile Kadenz-Anzeige ohne Textspringen darstellt.
  pub fn observe_fg_phase(&mut self, fg_active: bool, is_interpolated_phase: bool) {
    if fg_active {
      self.fg_phase_window = (self.fg_phase_window << 1) | u32::from(is_interpolated_phase);
      if is_interpolated_phase {
        self.fg_interpolated_frames = self.fg_interpolated_frames.wrapping_add(1);
      }
    } else {
      self.fg_phase_window = 0;
    }
  }

  pub fn has_verified_fg_cadence(&self) -> bool {
    self.fg_phase_window != 0 && (self.fg_interpolated_frames > 0)
  }

  /// Ungedrosselte Echtzeit-Frametime fuer die interne DX12-Shader-/SDK-Temporalglaettung.
  pub fn live_delta_time_ms(&self) -> f32 {
    self.live_dt_ms
  }

  pub fn latched_d3d12_fps(&self) -> f32 {
    self.latched.avg_d3d12_fps
  }

  pub fn latched_frametime_ms(&self) -> f32 {
    self.latched.avg_frametime_ms
  }

  pub fn latched_gbuffer_gpu_ms(&self) -> f32 {
    self.latched.gbuf_gpu_ms
  }

  pub fn latched_fg_gpu_ms(&self) -> f32 {
    self.latched.fg_gpu_ms
  }

  pub fn latched_capture_raw_fps(&self) -> f32 {
    self.latched.in_raw_fps
  }

  pub fn latched_capture_unique_fps(&self) -> f32 {
    self.latched.in_unique_fps
  }

  pub fn latched_capture_dup_pct(&self) -> f32 {
    self.latched.duplicate_ratio_pct
  }

  pub fn latched_effective_video_fps(&self) -> f32 {
    self.latched.effective_video_fps
  }

  pub fn latched_audio_latency_ms(&self) -> f32 {
    self.latched.audio_latency_ms
  }

  pub fn fps_1_percent_low(&self) -> f32 {
    self.latched.low_1_fps
  }

  pub fn fps_0_1_percent_low(&self) -> f32 {
    self.latched.low_01_fps
  }

  pub fn cpu_usage_percent(&self) -> f32 {
    self.cpu_pct
  }

  pub fn ram_used_gb(&self) -> f32 {
    self.ram_used_gb
  }

  pub fn ram_total_gb(&self) -> f32 {
    self.ram_total_gb
  }

  pub fn process_ram_mb(&self) -> f32 {
    self.proc_ram_mb
  }

  pub fn vram_info(&self) -> VramUsageInfo {
    self.vram
  }

  fn compute_lows(&mut self) -> (f32, f32) {
    if self.ft_win.is_empty() {
      return (0.0_f32, 0.0_f32);
    }
    self.scratch.clear();
    self.scratch.extend(self.ft_win.iter().copied());
    self
      .scratch
      .sort_unstable_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let n = self.scratch.len();
    let (c1, c01) = ((n / 100).max(1), (n / 1000).max(1));
    let a1 = self.scratch[..c1].iter().sum::<f32>() / (c1 as f32);
    let a01 = self.scratch[..c01].iter().sum::<f32>() / (c01 as f32);
    let low_1 = if a1 > 0.001_f32 {
      1000.0_f32 / a1
    } else {
      0.0_f32
    };
    let low_01 = if a01 > 0.001_f32 {
      1000.0_f32 / a01
    } else {
      0.0_f32
    };
    (low_1, low_01)
  }

  fn poll_cpu(&mut self) {
    let Some((i, k, u)) = sample_cpu_ticks() else {
      return;
    };
    let (di, dk, du) = (
      i.saturating_sub(self.prev_ticks.0),
      k.saturating_sub(self.prev_ticks.1),
      u.saturating_sub(self.prev_ticks.2),
    );
    self.prev_ticks = (i, k, u);
    let tot = dk.saturating_add(du);
    if tot > 0 {
      let pct = (((tot.saturating_sub(di) as f64) * 100.0_f64 / (tot as f64)) as f32)
        .clamp(0.0_f32, 100.0_f32);
      self.cpu_pct = if self.cpu_pct <= 0.001_f32 {
        pct
      } else {
        self.cpu_pct * 0.65_f32 + pct * 0.35_f32
      };
    }
  }

  fn poll_mem(&mut self) {
    unsafe {
      let mut ms = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
      };
      if GlobalMemoryStatusEx(&mut ms).is_ok() && ms.ullTotalPhys > 0 {
        let (tot, avail, gib) = (
          ms.ullTotalPhys as f64,
          ms.ullAvailPhys.min(ms.ullTotalPhys) as f64,
          1024.0_f64.powi(3),
        );
        self.ram_total_gb = (tot / gib) as f32;
        self.ram_used_gb = ((tot - avail) / gib) as f32;
      }
      let mut pmc = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
      };
      if K32GetProcessMemoryInfo(
        GetCurrentProcess(),
        &mut pmc,
        size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
      )
      .as_bool()
      {
        self.proc_ram_mb = (pmc.WorkingSetSize as f64 / (1024.0_f64 * 1024.0_f64)) as f32;
      }
    }
  }
}

fn ft_u64(ft: FILETIME) -> u64 {
  ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn sample_cpu_ticks() -> Option<(u64, u64, u64)> {
  let (mut i, mut k, mut u) = (
    FILETIME::default(),
    FILETIME::default(),
    FILETIME::default(),
  );
  unsafe { GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)).ok()? };
  Some((ft_u64(i), ft_u64(k), ft_u64(u)))
}

pub fn shorten_gpu_name(raw: &str) -> String {
  let c = raw
    .replace("(R)", "")
    .replace("(TM)", "")
    .replace("(tm)", "")
    .replace("Graphics", "")
    .replace("NVIDIA ", "")
    .replace("Advanced Micro Devices, Inc.", "AMD");
  let j = c.split_whitespace().collect::<Vec<&str>>().join(" ");
  if j.is_empty() {
    String::from("DX12 GPU")
  } else if j.chars().count() > 24 {
    j.chars().take(24).collect()
  } else {
    j
  }
}

/// Liefert eine exakt 10 Zeichen lange, laengenstabile Kurzkennung fuer `PIPE`
/// (z. B. `FSR 4.0 2x`, `FSR 3.1 2x`, `XeSS 3  3x`, `DLSS4.5 4x` oder `*-CS <M>x`).
fn fg_compact_badge(
  backend: UpscalerBackend,
  native_sdk: bool,
  status_label: &str,
  multiplier: u32,
  cadence_locked: bool,
) -> String {
  let m = if cadence_locked {
    multiplier.clamp(2, 4)
  } else {
    1
  };
  let prefix7 = match backend {
    UpscalerBackend::Passthrough => return String::from("Passthru  "),
    UpscalerBackend::AmdFsr3 => {
      if native_sdk {
        if status_label.contains("4.1") {
          "FSR 4.1"
        } else if status_label.contains("4.") {
          "FSR 4.0"
        } else if status_label.contains("3.0") {
          "FSR 3.0"
        } else {
          "FSR 3.1"
        }
      } else {
        "FSR-CS "
      }
    }
    UpscalerBackend::IntelXeSSFg => {
      if native_sdk {
        "XeSS 3 "
      } else {
        "XeSS-CS"
      }
    }
    UpscalerBackend::NvidiaDlssFg => {
      if native_sdk {
        if status_label.contains("DLSS 5") {
          "DLSS 5 "
        } else {
          "DLSS4.5"
        }
      } else if status_label.contains("Streamline")
        && !status_label.contains("Fallback")
        && !status_label.contains("Turbo")
      {
        "SL-CS  "
      } else {
        "DLSS-CS"
      }
    }
  };
  format!("{prefix7} {m}x")
}

fn fg_inactive_badge(backend: UpscalerBackend) -> &'static str {
  match backend {
    UpscalerBackend::Passthrough => "Passthru  ",
    UpscalerBackend::AmdFsr3 => "FSR 4.1   ",
    UpscalerBackend::IntelXeSSFg => "XeSS 3    ",
    UpscalerBackend::NvidiaDlssFg => "DLSS 4.5  ",
  }
}

fn resolve_latched_gbuffer_metrics(
  tel: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
) -> ((u32, u32), f32) {
  let dims = if rt.gbuffer_dims.0 > 0 && rt.gbuffer_dims.1 > 0 {
    rt.gbuffer_dims
  } else {
    (rt.gbuffer.width, rt.gbuffer.height)
  };
  let ms = if rt.gbuffer_gpu_ms > 0.0_f32 {
    rt.gbuffer_gpu_ms
  } else {
    tel.latched_gbuffer_gpu_ms()
  };
  (dims, ms)
}

fn format_pipe_row(
  tel: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
  default_val_col: Color32,
  lang: AppLanguage,
) -> (String, String, Color32) {
  let hud_tag = if rt.turbo.hud_protection {
    tr(lang, " | HUD-Schutz", " | HUD-Guard")
  } else {
    ""
  };
  if rt.backend != UpscalerBackend::Passthrough && rt.frame_gen_enabled {
    let cadence_ok =
      !rt.capture_has_signal || tel.has_verified_fg_cadence() || rt.is_interpolated_phase;
    let mult = if rt.backend == UpscalerBackend::AmdFsr3 {
      2
    } else {
      rt.fg_multiplier.clamp(2, 4)
    };
    let badge = fg_compact_badge(
      rt.backend,
      rt.fg_native_sdk,
      &rt.fg_status_label,
      mult,
      cadence_ok,
    );
    if rt.capture_has_signal {
      let col = if rt.fg_native_sdk {
        rgb(110, 231, 183)
      } else {
        rgb(125, 211, 252)
      };
      let sec = format!(
        "| {:5.1}v | FG {:4.2}ms | CAS {:3.0}%{hud_tag}",
        rt.effective_video_fps,
        rt.fg_gpu_ms.clamp(0.0_f32, 99.99_f32),
        rt.cas_sharpness
      );
      (badge, sec, col)
    } else {
      (
        badge,
        format!(
          "| {}    | CAS {:3.0}%{hud_tag}",
          tr(lang, "Bereit ", "Standby"),
          rt.cas_sharpness
        ),
        rgb(148, 163, 184),
      )
    }
  } else {
    (
      String::from(fg_inactive_badge(rt.backend)),
      format!(
        "| {}    | CAS {:3.0}%{hud_tag}",
        tr(lang, "FG Aus ", "FG Off "),
        rt.cas_sharpness
      ),
      default_val_col,
    )
  }
}

fn format_pipe_compact(
  tel: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
  default_val_col: Color32,
  lang: AppLanguage,
) -> (String, Color32) {
  let hud_tag = if rt.turbo.hud_protection {
    tr(lang, " | HUD-Schutz", " | HUD-Guard")
  } else {
    ""
  };
  if rt.backend != UpscalerBackend::Passthrough && rt.frame_gen_enabled {
    let cadence_ok =
      !rt.capture_has_signal || tel.has_verified_fg_cadence() || rt.is_interpolated_phase;
    let mult = if rt.backend == UpscalerBackend::AmdFsr3 {
      2
    } else {
      rt.fg_multiplier.clamp(2, 4)
    };
    let badge = fg_compact_badge(
      rt.backend,
      rt.fg_native_sdk,
      &rt.fg_status_label,
      mult,
      cadence_ok,
    );
    if rt.capture_has_signal {
      let col = if rt.fg_native_sdk {
        rgb(110, 231, 183)
      } else {
        rgb(125, 211, 252)
      };
      (
        format!(
          "{badge} | {:5.1}v FPS | FG {:4.2}ms | CAS {:3.0}%{hud_tag}",
          rt.effective_video_fps,
          rt.fg_gpu_ms.clamp(0.0_f32, 99.99_f32),
          rt.cas_sharpness
        ),
        col,
      )
    } else {
      (
        format!(
          "{badge} | {} | CAS {:3.0}%{hud_tag}",
          tr(lang, "Bereit", "Standby"),
          rt.cas_sharpness
        ),
        rgb(148, 163, 184),
      )
    }
  } else {
    (
      format!(
        "{} | {} | CAS {:3.0}%{hud_tag}",
        rt.backend.hud_badge(),
        tr(lang, "FG Aus", "FG Off"),
        rt.cas_sharpness
      ),
      default_val_col,
    )
  }
}

/// Zeichnet das frei per Maus verschiebbare Performance-Overlay (OSD) mit 100 % fester,
/// zitterfreier Kastengroesse, zweisprachigen Statusbegriffen (`DE` / `EN`) und gibt `true`
/// zurueck, sobald `osd.pos_norm` per Maus-Drag verschoben wurde (fuer sofortige Persistenz).
pub fn draw_performance_osd(
  ctx: &Context,
  osd: &mut OsdConfig,
  telemetry: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
  lang: AppLanguage,
) -> bool {
  if !osd.enabled || (osd.hide_in_menu && rt.menu_visible) {
    return false;
  }
  let scale = (osd.scale_percent / 100.0_f32).clamp(0.75_f32, 1.60_f32);
  let base_box_w = osd.box_width.clamp(320.0_f32, 620.0_f32);
  let fixed_w = if osd.preset == OsdPreset::CompactOneLine {
    (base_box_w * 1.35_f32) * scale
  } else {
    base_box_w * scale
  };

  let op = (osd.background_opacity_percent / 100.0_f32).clamp(0.0_f32, 0.90_f32);
  let bg_a = (op * 255.0_f32).round() as u8;
  let bd_a = if bg_a < 12 {
    0_u8
  } else {
    ((op * 195.0_f32).round() as u8).max(32)
  };
  let (pad_x, pad_y) = if bg_a == 0 {
    (2.0_f32, 2.0_f32)
  } else {
    (10.0_f32 * scale, 7.5_f32 * scale)
  };

  let screen = ctx.screen_rect();
  let (sw, sh) = (screen.width().max(1.0_f32), screen.height().max(1.0_f32));
  let clamped_outer_w = (fixed_w + 2.0_f32 * pad_x).clamp(80.0_f32, sw);
  let est_h = osd.last_size_pts[1].clamp(20.0_f32, sh);
  let (min_x, min_y) = (screen.left() + 4.0_f32, screen.top() + 4.0_f32);
  let (max_x, max_y) = (
    (screen.right() - clamped_outer_w - 4.0_f32).max(min_x),
    (screen.bottom() - est_h - 4.0_f32).max(min_y),
  );
  let px = (screen.left() + osd.pos_norm[0].clamp(0.0_f32, 1.0_f32) * sw).clamp(min_x, max_x);
  let py = (screen.top() + osd.pos_norm[1].clamp(0.0_f32, 1.0_f32) * sh).clamp(min_y, max_y);

  let mut pos_changed = false;

  Area::new(Id::new("ff_performance_osd"))
    .fixed_pos(pos2(px, py))
    .order(Order::Foreground)
    .interactable(!osd.locked)
    .show(ctx, |ui| {
      let frame_resp = Frame::default()
        .fill(rgba(8, 11, 18, bg_a))
        .stroke(Stroke::new(1.0_f32, rgba(56, 78, 112, bd_a)))
        .rounding(Rounding::same(6.0_f32))
        .inner_margin(Margin::symmetric(pad_x, pad_y))
        .show(ui, |ui| {
          ui.style_mut().wrap_mode = Some(TextWrapMode::Truncate);
          if osd.preset == OsdPreset::CompactOneLine {
            let fixed_w = (base_box_w * 1.35_f32) * scale;
            ui.set_min_width(fixed_w);
            ui.set_max_width(fixed_w);
            draw_compact(ui, osd, telemetry, rt, scale, fixed_w, lang);
          } else {
            let fixed_w = base_box_w * scale;
            ui.set_min_width(fixed_w);
            ui.set_max_width(fixed_w);
            draw_table(ui, osd, telemetry, rt, scale, fixed_w, lang);
          }
        });

      let osd_rect = frame_resp.response.rect;
      osd.last_size_pts = [clamped_outer_w, osd_rect.height()];

      if !osd.locked {
        let drag_resp = ui.interact(osd_rect, Id::new("ff_osd_drag"), Sense::drag());
        if drag_resp.dragged() {
          ctx.set_cursor_icon(CursorIcon::Grabbing);
          let d = drag_resp.drag_delta();
          if d.length_sq() > 0.0_f32 {
            let (cx, cy) = (
              (screen.right() - clamped_outer_w - 4.0_f32).max(min_x),
              (screen.bottom() - osd_rect.height() - 4.0_f32).max(min_y),
            );
            let (nx, ny) = ((px + d.x).clamp(min_x, cx), (py + d.y).clamp(min_y, cy));
            let next_norm = [
              ((nx - screen.left()) / sw).clamp(0.0_f32, 1.0_f32),
              ((ny - screen.top()) / sh).clamp(0.0_f32, 1.0_f32),
            ];
            if (next_norm[0] - osd.pos_norm[0]).abs() > 1e-6_f32
              || (next_norm[1] - osd.pos_norm[1]).abs() > 1e-6_f32
            {
              osd.pos_norm = next_norm;
              pos_changed = true;
            }
          }
          ui.painter().rect_stroke(
            osd_rect.expand(1.0_f32),
            Rounding::same(7.0_f32),
            Stroke::new(1.5_f32, osd.color_theme.highlight_color()),
          );
        } else if drag_resp.hovered() {
          ctx.set_cursor_icon(CursorIcon::Grab);
          ui.painter().rect_stroke(
            osd_rect.expand(1.0_f32),
            Rounding::same(7.0_f32),
            Stroke::new(1.2_f32, osd.color_theme.accent_color()),
          );
        }
      }
    });

  pos_changed
}

fn draw_compact(
  ui: &mut Ui,
  osd: &OsdConfig,
  tel: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
  scale: f32,
  fixed_w: f32,
  lang: AppLanguage,
) {
  let font = FontId::monospace(12.5_f32 * scale);
  let (acc, hi, val, sep) = (
    osd.color_theme.accent_color(),
    osd.color_theme.highlight_color(),
    osd.color_theme.value_color(),
    rgb(120, 138, 165),
  );
  let row_right = ui.cursor().min.x + fixed_w;
  ui.horizontal(|ui| {
    ui.set_min_width(fixed_w);
    ui.set_max_width(fixed_w);
    ui.spacing_mut().item_spacing.x = 6.0_f32 * scale;
    sh_lbl_clamped(ui, "D3D12", &font, hi, row_right);
    sh_lbl_clamped(
      ui,
      &format!(
        "{:4.0} FPS ({:5.2}ms)",
        tel.latched_d3d12_fps(),
        tel.latched_frametime_ms()
      ),
      &font,
      val,
      row_right,
    );
    if osd.rows.fps_percentiles {
      sh_lbl_clamped(ui, "|", &font, sep, row_right);
      sh_lbl_clamped(
        ui,
        &format!("1% {:4.0}", tel.fps_1_percent_low()),
        &font,
        osd.color_theme.unit_color(),
        row_right,
      );
    }
    if osd.rows.capture_signal {
      sh_lbl_clamped(ui, "|", &font, sep, row_right);
      sh_lbl_clamped(ui, "CAP", &font, acc, row_right);
      sh_lbl_clamped(
        ui,
        &(if rt.capture_has_signal {
          format!("{:5.1} FPS", rt.capture_unique_fps)
        } else {
          String::from(tr(lang, "  Bereit", " Standby"))
        }),
        &font,
        val,
        row_right,
      );
    }
    if osd.rows.gbuffer_compute {
      sh_lbl_clamped(ui, "|", &font, sep, row_right);
      sh_lbl_clamped(ui, "GBUF", &font, acc, row_right);
      let ((gb_w, gb_h), gb_ms) = resolve_latched_gbuffer_metrics(tel, rt);
      let s = if !rt.gbuffer.enabled {
        String::from(tr(lang, "AUS", "OFF"))
      } else if rt.gbuffer.active && gb_w > 0 && gb_h > 0 {
        if rt.debug_view_mode != DebugViewMode::Normal {
          format!(
            "{gb_ms:5.2}ms ({gb_w}x{gb_h} {})",
            rt.debug_view_mode.short_badge()
          )
        } else {
          format!("{gb_ms:5.2}ms ({gb_w}x{gb_h})")
        }
      } else {
        String::from(tr(lang, "Bereit", "Standby"))
      };
      sh_lbl_clamped(ui, &s, &font, val, row_right);
    }
    if osd.rows.pipeline_backend {
      sh_lbl_clamped(ui, "|", &font, sep, row_right);
      sh_lbl_clamped(ui, "PIPE", &font, acc, row_right);
      let (pipe_txt, pipe_col) = format_pipe_compact(tel, rt, val, lang);
      sh_lbl_clamped(ui, &pipe_txt, &font, pipe_col, row_right);
    }
    if osd.rows.audio_engine {
      sh_lbl_clamped(ui, "|", &font, sep, row_right);
      sh_lbl_clamped(ui, "AUD", &font, acc, row_right);
      let s = if !rt.audio.enabled {
        String::from(tr(lang, "AUS  ", "OFF  "))
      } else if rt.audio.muted {
        String::from(tr(lang, "Stumm", "Muted"))
      } else if rt.audio.active_stream_ok {
        format!("{:3.0}ms", tel.latched_audio_latency_ms())
      } else {
        String::from(tr(lang, "Bereit", "Standby"))
      };
      sh_lbl_clamped(ui, &s, &font, val, row_right);
    }
  });
}

fn draw_table(
  ui: &mut Ui,
  osd: &OsdConfig,
  tel: &SystemTelemetry,
  rt: &OsdRuntimeContext<'_>,
  scale: f32,
  fixed_w: f32,
  lang: AppLanguage,
) {
  let (rfont, hfont, sfont) = (
    FontId::monospace(12.0_f32 * scale),
    FontId::monospace(14.5_f32 * scale),
    FontId::monospace(10.5_f32 * scale),
  );
  let (acc, hi, val, unit) = (
    osd.color_theme.accent_color(),
    osd.color_theme.highlight_color(),
    osd.color_theme.value_color(),
    osd.color_theme.unit_color(),
  );
  let tw = 52.0_f32 * scale;

  ui.vertical(|ui| {
    ui.set_min_width(fixed_w);
    ui.set_max_width(fixed_w);
    ui.spacing_mut().item_spacing.y = 2.5_f32 * scale;
    let mut any = false;
    if osd.rows.gpu_and_vram {
      any = true;
      let sync = if rt.allow_tearing_active {
        "Tearing"
      } else {
        "VSync  "
      };
      draw_row(
        ui,
        fixed_w,
        tw,
        "GPU",
        &shorten_gpu_name(rt.adapter_name),
        &format!("{}x{} {sync}", rt.swapchain_size.0, rt.swapchain_size.1),
        &rfont,
        acc,
        val,
        unit,
        scale,
      );
      let v = tel.vram_info();
      let (mv, mu) = if v.is_integrated_shared {
        (
          format!("{:4} MB", v.local_used_mb.saturating_add(v.shared_used_mb)),
          format!(
            "{} ({} {:4} / Sys {:4})",
            tr(lang, "Geteilt", "Shared"),
            tr(lang, "Lok", "Loc"),
            v.local_used_mb,
            v.shared_used_mb
          ),
        )
      } else if v.shared_used_mb > 0 {
        (
          format!("{:4} MB", v.local_used_mb),
          format!(
            "VRAM (+{:4} MB {})",
            v.shared_used_mb,
            tr(lang, "Geteilt", "Shared")
          ),
        )
      } else {
        (
          format!("{:4} MB", v.local_used_mb),
          format!("/ {:4} MB Budget", v.local_budget_mb),
        )
      };
      draw_row(ui, fixed_w, tw, "MEM", &mv, &mu, &rfont, acc, val, unit, scale);
    }
    if osd.rows.cpu_usage {
      any = true;
      draw_row(
        ui,
        fixed_w,
        tw,
        "CPU",
        &format!("{:5.1} %", tel.cpu_usage_percent()),
        tr(lang, "System Gesamt", "System Total"),
        &rfont,
        acc,
        val,
        unit,
        scale,
      );
    }
    if osd.rows.system_and_app_ram {
      any = true;
      draw_row(
        ui,
        fixed_w,
        tw,
        "RAM",
        &format!("{:4.1} / {:4.1} GB", tel.ram_used_gb(), tel.ram_total_gb()),
        &format!("App {:4.0} MB", tel.process_ram_mb()),
        &rfont,
        acc,
        val,
        unit,
        scale,
      );
    }
    if osd.rows.capture_signal {
      any = true;
      let rng = if rt.expand_limited_rgb {
        "Lim 16-235"
      } else {
        tr(lang, "Voll 0-255", "Full 0-255")
      };
      if rt.capture_has_signal {
        let res = rt
          .capture_resolution
          .map(|(w, h)| format!("{w}x{h}"))
          .unwrap_or_else(|| String::from("Live"));
        let det = if rt.duplicate_filter_active {
          format!(
            "In {:5.1} -> {:5.1} FPS | Dup {:3.0}%",
            rt.capture_raw_fps, rt.capture_unique_fps, rt.capture_duplicate_ratio_pct
          )
        } else {
          format!(
            "In {:5.1} FPS | Dup {:3.0}%",
            rt.capture_raw_fps, rt.capture_duplicate_ratio_pct
          )
        };
        draw_row(
          ui,
          fixed_w,
          tw,
          "CAP",
          &format!("{res} {}", rt.capture_format_label),
          &format!("{det} | {rng}"),
          &rfont,
          acc,
          rgb(134, 239, 172),
          unit,
          scale,
        );
      } else {
        draw_row(
          ui,
          fixed_w,
          tw,
          "CAP",
          tr(lang, "Kein Signal", "No Signal"),
          &format!("{} | {rng}", tr(lang, "Bereit", "Standby")),
          &rfont,
          acc,
          rgb(148, 163, 184),
          unit,
          scale,
        );
      }
    }
    if osd.rows.gbuffer_compute {
      any = true;
      let ((gb_w, gb_h), gb_ms) = resolve_latched_gbuffer_metrics(tel, rt);
      let sfx = if rt.debug_view_mode != DebugViewMode::Normal {
        format!(" | {}", rt.debug_view_mode.short_badge())
      } else {
        String::new()
      };
      if !rt.gbuffer.enabled {
        draw_row(
          ui,
          fixed_w,
          tw,
          "GBUF",
          &format!(" 0.00ms | {}", tr(lang, "AUS", "OFF")),
          &format!("{}{sfx}", tr(lang, "Deaktiviert", "Disabled")),
          &rfont,
          acc,
          rgb(148, 163, 184),
          unit,
          scale,
        );
      } else if rt.gbuffer.active && gb_w > 0 && gb_h > 0 {
        let flow50 = rt.turbo.half_res_flow_scale || rt.gbuffer.half_res_flow_scale;
        let fast = rt.turbo.fast_gbuffer_search || rt.gbuffer.fast_search;
        let mode_tag = match (flow50, fast) {
          (true, true) => "Flow50% Fast",
          (true, false) => "Flow50%     ",
          (false, true) => "Fast-5Tap   ",
          (false, false) => "Edge-Aegis  ",
        };
        draw_row(
          ui,
          fixed_w,
          tw,
          "GBUF",
          &format!("{gb_ms:5.2}ms | {}", tr(lang, "EIN", "ON ")),
          &format!("{gb_w}x{gb_h} {mode_tag}{sfx}"),
          &rfont,
          acc,
          rgb(125, 211, 252),
          unit,
          scale,
        );
      } else {
        draw_row(
          ui,
          fixed_w,
          tw,
          "GBUF",
          &format!(" 0.00ms | {}", tr(lang, "EIN", "ON ")),
          &format!("{}{sfx}", tr(lang, "Bereit", "Standby")),
          &rfont,
          acc,
          rgb(148, 163, 184),
          unit,
          scale,
        );
      }
    }
    if osd.rows.pipeline_backend {
      any = true;
      let (pipe_pri, pipe_sec, pipe_col) = format_pipe_row(tel, rt, val, lang);
      draw_row(
        ui, fixed_w, tw, "PIPE", &pipe_pri, &pipe_sec, &rfont, acc, pipe_col, unit, scale,
      );
    }
    if osd.rows.audio_engine {
      any = true;
      let (st, sc, dt) = if !rt.audio.enabled {
        (
          tr(lang, "AUS  ", "OFF  "),
          rgb(148, 163, 184),
          String::from(tr(lang, "Audio deaktiviert", "Audio disabled")),
        )
      } else if rt.audio.muted {
        (
          tr(lang, "Stumm", "Muted"),
          rgb(251, 191, 36),
          format!(
            "{:4.1} kHz | Vol {:3.0}%",
            (rt.audio.sample_rate_hz as f32) / 1000.0_f32,
            rt.audio.volume * 100.0_f32
          ),
        )
      } else if rt.audio.active_stream_ok {
        (
          tr(lang, "EIN  ", "ON   "),
          rgb(134, 239, 172),
          format!(
            "{:4.1} kHz | {:5.1}ms | Vol {:3.0}%",
            (rt.audio.sample_rate_hz as f32) / 1000.0_f32,
            tel.latched_audio_latency_ms(),
            rt.audio.volume * 100.0_f32
          ),
        )
      } else {
        (
          tr(lang, "Bereit", "Standby"),
          rgb(251, 191, 36),
          String::from(tr(
            lang,
            "Kein Capture-Audiostream",
            "No capture audio stream",
          )),
        )
      };
      draw_row(ui, fixed_w, tw, "AUD", st, &dt, &rfont, acc, sc, unit, scale);
    }
    if osd.rows.output_fps_frametime {
      if any {
        ui.add_space(2.0_f32 * scale);
      }
      any = true;
      draw_row(
        ui,
        fixed_w,
        tw,
        "D3D12",
        &format!("{:4.0} FPS", tel.latched_d3d12_fps()),
        &format!("{:5.2} ms", tel.latched_frametime_ms()),
        &hfont,
        hi,
        Color32::WHITE,
        hi,
        scale,
      );
    }
    if osd.rows.fps_percentiles {
      any = true;
      draw_row(
        ui,
        fixed_w,
        tw,
        "LOWS",
        &format!("1% {:4.0} FPS", tel.fps_1_percent_low()),
        &format!("0.1% {:4.0} FPS", tel.fps_0_1_percent_low()),
        &rfont,
        acc,
        val,
        unit,
        scale,
      );
    }
    if osd.rows.frametime_graph {
      if any {
        ui.add_space(3.0_f32 * scale);
      }
      any = true;
      draw_ft_graph(ui, tel, osd, &sfont, scale, fixed_w);
    }
    if !any {
      let row_right = ui.cursor().min.x + fixed_w;
      sh_lbl_clamped(
        ui,
        tr(
          lang,
          "Telemetrie-OSD aktiv (keine Zeile aktiviert)",
          "Telemetry OSD active (no rows enabled)",
        ),
        &sfont,
        unit,
        row_right,
      );
    }
  });
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
  ui: &mut Ui,
  fixed_w: f32,
  tw: f32,
  tag: &str,
  pri: &str,
  sec: &str,
  font: &FontId,
  tc: Color32,
  vc: Color32,
  uc: Color32,
  scale: f32,
) {
  let row_right = ui.cursor().min.x + fixed_w;
  let row_h = (font.size * 1.24_f32).ceil();
  ui.horizontal(|ui| {
    ui.set_min_width(fixed_w);
    ui.set_max_width(fixed_w);
    ui.spacing_mut().item_spacing.x = 6.0_f32 * scale;
    let (tr_rect, _) = ui.allocate_exact_size(Vec2::new(tw.min(fixed_w), row_h), Sense::hover());
    sh_at_clipped(
      ui,
      pos2(tr_rect.left(), tr_rect.top()),
      tag,
      font,
      tc,
      tr_rect.expand2(Vec2::new(2.0_f32, 2.0_f32)),
    );
    sh_lbl_clamped(ui, pri, font, vc, row_right);
    if !sec.is_empty() {
      sh_lbl_clamped(ui, sec, font, uc, row_right);
    }
  });
}

fn draw_ft_graph(
  ui: &mut Ui,
  tel: &SystemTelemetry,
  osd: &OsdConfig,
  sfont: &FontId,
  scale: f32,
  fixed_w: f32,
) {
  let hist = &tel.graph_ms;
  let mn = tel.latched.ft_min_ms;
  let mx = tel.latched.ft_max_ms;
  let top_ms = tel.graph_top_ms_smoothed.clamp(6.0_f32, 66.7_f32);
  let row_right = ui.cursor().min.x + fixed_w;

  ui.horizontal(|ui| {
    ui.set_min_width(fixed_w);
    ui.set_max_width(fixed_w);
    ui.spacing_mut().item_spacing.x = 8.0_f32 * scale;
    sh_lbl_clamped(
      ui,
      &format!("FT min {:5.2} ms", mn),
      sfont,
      osd.color_theme.unit_color(),
      row_right,
    );
    sh_lbl_clamped(
      ui,
      &format!("max {:5.2} ms", mx),
      sfont,
      osd.color_theme.highlight_color(),
      row_right,
    );
  });

  let (rect, _) = ui.allocate_exact_size(
    Vec2::new(fixed_w, (36.0_f32 * scale).max(26.0_f32).ceil()),
    Sense::hover(),
  );
  let p = ui.painter();
  p.rect_filled(rect, Rounding::same(3.0_f32), rgba(4, 7, 13, 120));
  p.rect_stroke(
    rect,
    Rounding::same(3.0_f32),
    Stroke::new(1.0_f32, rgba(90, 110, 140, 115)),
  );
  if top_ms >= 16.67_f32 {
    let ry = rect.bottom() - (16.67_f32 / top_ms).clamp(0.0_f32, 1.0_f32) * rect.height();
    p.line_segment(
      [pos2(rect.left() + 1.0_f32, ry), pos2(rect.right() - 1.0_f32, ry)],
      Stroke::new(1.0_f32, rgba(148, 163, 184, 75)),
    );
  }
  let len = hist.len();
  if len < 2 {
    return;
  }
  let u = Rect::from_min_max(
    pos2(rect.left() + 2.0_f32, rect.top() + 2.0_f32),
    pos2(rect.right() - 2.0_f32, rect.bottom() - 2.0_f32),
  );
  let sx = u.width() / ((GRAPH_CAP.max(len) - 1).max(1) as f32);
  let x0 = u.right() - sx * ((len - 1) as f32);
  let (mut sp, mut lp) = (Vec::with_capacity(len), Vec::with_capacity(len));
  for (i, &ms) in hist.iter().enumerate() {
    let (x, y) = (
      x0 + (i as f32) * sx,
      u.bottom() - (ms / top_ms).clamp(0.0_f32, 1.0_f32) * u.height(),
    );
    sp.push(pos2(x + 1.0_f32, y + 1.0_f32));
    lp.push(pos2(x, y));
  }
  p.add(Shape::line(sp, Stroke::new(1.8_f32, rgba(0, 0, 0, 210))));
  p.add(Shape::line(
    lp,
    Stroke::new(1.4_f32, osd.color_theme.highlight_color()),
  ));
}

fn sh_lbl_clamped(ui: &mut Ui, text: &str, font: &FontId, color: Color32, max_right: f32) {
  let avail_w = (max_right - ui.cursor().min.x).max(0.0_f32);
  let row_h = (font.size * 1.24_f32).ceil();
  if avail_w <= 1.5_f32 {
    return;
  }
  let sz = ui
    .fonts(|f| f.layout_no_wrap(text.to_string(), font.clone(), color))
    .size();
  let alloc_w = sz.x.min(avail_w);
  let (rect, _) = ui.allocate_exact_size(Vec2::new(alloc_w, row_h), Sense::hover());
  let clip = Rect::from_min_max(rect.min, pos2(max_right, rect.bottom() + 2.0_f32));
  sh_at_clipped(ui, rect.min, text, font, color, clip);
}

fn sh_at_clipped(ui: &mut Ui, pos: Pos2, text: &str, font: &FontId, color: Color32, clip: Rect) {
  let g = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font.clone(), color));
  let sh = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font.clone(), rgba(0, 0, 0, 235)));
  let p = ui.painter().with_clip_rect(ui.clip_rect().intersect(clip));
  p.galley(pos + Vec2::new(1.0_f32, 1.0_f32), sh.clone(), Color32::BLACK);
  p.galley(pos + Vec2::new(1.5_f32, 1.5_f32), sh, Color32::BLACK);
  p.galley(pos, g, color);
}
