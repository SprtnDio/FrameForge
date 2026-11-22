use std::time::Instant;

use egui::{
  Align, Button, CentralPanel, Checkbox, Color32, ComboBox, Context, CursorIcon, FontId, Frame,
  Layout, Margin, Rect, RichText, Rounding, ScrollArea, Sense, SidePanel, Slider, Stroke,
  TextureHandle, TopBottomPanel, Ui, Vec2, Visuals,
};

use crate::audio::{
  find_preferred_capture_audio_index, AudioDeviceInfo, AudioLiveTelemetry, AudioRouteConfig,
};
use crate::capture::{CaptureConfig, CaptureDeviceInfo};
use crate::config::FPS_LIMIT_VALUES;
use crate::dx12::PerformanceTurboConfig;
use crate::gbuffer::{GBufferParams, GBufferStats};
use crate::i18n::{tr, AppLanguage};
use crate::logo::{draw_brand_logo, force_refresh_windows_icon_cache};
use crate::telemetry::{
  draw_performance_osd, osd_refresh_interval_label, shorten_gpu_name, OsdColorTheme, OsdConfig,
  OsdPreset, OsdQuickSlot, OsdRowFlags, OsdRuntimeContext, SystemTelemetry,
  OSD_REFRESH_INTERVALS_MS,
};

/// UI-Reihenfolge fuer den D3D12-Ausgabe-FPS-Limiter (`Auto` zuerst, `Unbegrenzt` zuletzt).
pub const UI_FPS_LIMIT_OPTIONS: [u32; 6] = [1, 60, 120, 144, 240, 0];

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

fn lbl12(ui: &mut Ui, s: &str) {
  ui.label(RichText::new(s).size(12.0_f32).color(rgb(203, 213, 225)));
}

fn enum_combo_lang<T: Copy + PartialEq>(
  ui: &mut Ui,
  id: &str,
  w: f32,
  cur: &mut T,
  all: &[T],
  lang: AppLanguage,
  label: fn(T, AppLanguage) -> &'static str,
) {
  ComboBox::from_id_salt(id)
    .width(w)
    .selected_text(label(*cur, lang))
    .show_ui(ui, |ui| {
      for &item in all {
        ui.selectable_value(cur, item, label(item, lang));
      }
    });
}

pub fn output_fps_limit_label(limit: u32, lang: AppLanguage) -> &'static str {
  match limit {
    1 => tr(lang, "Auto (Passend zu Video/FG)", "Auto (Match Video/FG)"),
    60 => "60 FPS",
    120 => "120 FPS",
    144 => "144 FPS",
    240 => "240 FPS",
    _ => tr(lang, "Unbegrenzt", "Uncapped"),
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
  SignalAndAudio,
  ImageAndFrameGen,
  GBuffer3D,
  PerformanceOsd,
  LogAndInfo,
}

impl SidebarTab {
  pub const ALL: [Self; 5] = [
    Self::SignalAndAudio,
    Self::ImageAndFrameGen,
    Self::GBuffer3D,
    Self::PerformanceOsd,
    Self::LogAndInfo,
  ];

  pub fn title(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::SignalAndAudio => tr(lang, "Signal & Audio", "Signal & Audio"),
      Self::ImageAndFrameGen => tr(lang, "Bild & Frame Gen", "Display & Frame Gen"),
      Self::GBuffer3D => tr(lang, "3D-G-Buffer", "3D G-Buffer"),
      Self::PerformanceOsd => tr(lang, "Performance-OSD", "Performance OSD"),
      Self::LogAndInfo => tr(lang, "Protokoll & Info", "Logs & About"),
    }
  }

  pub fn subtitle(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::SignalAndAudio => tr(lang, "HDMI-Video & WASAPI-Ton", "HDMI Video & WASAPI Audio"),
      Self::ImageAndFrameGen => tr(
        lang,
        "Multi-FG (2x/3x/4x), Turbo & Sync",
        "Multi-FG (2x/3x/4x), Turbo & Sync",
      ),
      Self::GBuffer3D => tr(lang, "Motion & Pseudo-Depth", "Motion & Pseudo-Depth"),
      Self::PerformanceOsd => tr(lang, "Telemetrie-Overlay", "Telemetry Overlay"),
      Self::LogAndInfo => tr(lang, "Version, Hotkeys & Log", "Version, Hotkeys & Logs"),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
  Info,
  Warn,
  Error,
}

impl LogLevel {
  pub fn tag(self) -> &'static str {
    match self {
      Self::Info => "INFO",
      Self::Warn => "WARN",
      Self::Error => "ERR ",
    }
  }

  pub fn color(self) -> Color32 {
    match self {
      Self::Info => rgb(125, 211, 252),
      Self::Warn => rgb(251, 191, 36),
      Self::Error => rgb(248, 113, 113),
    }
  }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
  pub timestamp_secs: f32,
  pub level: LogLevel,
  pub message: String,
}

pub struct LogStore {
  start_instant: Instant,
  entries: Vec<LogEntry>,
  max_entries: usize,
}

impl Default for LogStore {
  fn default() -> Self {
    Self::new()
  }
}

impl LogStore {
  pub fn new() -> Self {
    Self {
      start_instant: Instant::now(),
      entries: Vec::with_capacity(64),
      max_entries: 300,
    }
  }

  pub fn push(&mut self, level: LogLevel, message: impl Into<String>) {
    let timestamp_secs = self.start_instant.elapsed().as_secs_f32();
    let msg = message.into();
    #[cfg(debug_assertions)]
    eprintln!("[+{:6.2}s][{}] {}", timestamp_secs, level.tag(), msg);
    if self.entries.len() >= self.max_entries {
      self.entries.remove(0);
    }
    self.entries.push(LogEntry {
      timestamp_secs,
      level,
      message: msg,
    });
  }

  pub fn info(&mut self, msg: impl Into<String>) {
    self.push(LogLevel::Info, msg);
  }

  pub fn warn(&mut self, msg: impl Into<String>) {
    self.push(LogLevel::Warn, msg);
  }

  pub fn error(&mut self, msg: impl Into<String>) {
    self.push(LogLevel::Error, msg);
  }

  pub fn clear(&mut self) {
    self.entries.clear();
  }

  pub fn entries(&self) -> &[LogEntry] {
    &self.entries
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn format_all_plain(&self) -> String {
    let mut out = String::new();
    for e in &self.entries {
      out.push_str(&format!(
        "[+{:6.2}s] [{}] {}\n",
        e.timestamp_secs,
        e.level.tag(),
        e.message
      ));
    }
    out
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetResolution {
  Auto,
  P1080,
  P720,
}

impl TargetResolution {
  pub const ALL: [Self; 3] = [Self::Auto, Self::P1080, Self::P720];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::Auto => tr(lang, "Auto (Nativ / 1080p)", "Auto (Native / 1080p)"),
      Self::P1080 => "1080p (1920x1080)",
      Self::P720 => "720p (1280x720)",
    }
  }

  pub fn config_key(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::P1080 => "1080p",
      Self::P720 => "720p",
    }
  }

  pub fn from_config_key(key: &str) -> Self {
    match key.trim().to_ascii_lowercase().as_str() {
      "1080p" | "1920x1080" => Self::P1080,
      "720p" | "1280x720" => Self::P720,
      _ => Self::Auto,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFps {
  Fps60,
  Fps30,
}

impl InputFps {
  pub const ALL: [Self; 2] = [Self::Fps60, Self::Fps30];

  pub fn label(self, _lang: AppLanguage) -> &'static str {
    match self {
      Self::Fps60 => "60 FPS",
      Self::Fps30 => "30 FPS",
    }
  }

  pub fn as_u32(self) -> u32 {
    match self {
      Self::Fps60 => 60,
      Self::Fps30 => 30,
    }
  }

  pub fn from_u32(fps: u32) -> Self {
    if fps <= 30 {
      Self::Fps30
    } else {
      Self::Fps60
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpscalerBackend {
  Passthrough,
  AmdFsr3,
  IntelXeSSFg,
  NvidiaDlssFg,
}

impl UpscalerBackend {
  pub const ALL: [Self; 4] = [
    Self::Passthrough,
    Self::AmdFsr3,
    Self::IntelXeSSFg,
    Self::NvidiaDlssFg,
  ];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::Passthrough => tr(lang, "Aus (Passthrough)", "Off (Passthrough)"),
      Self::AmdFsr3 => "AMD FSR 4.1 / FG (SDK 2.4)",
      Self::IntelXeSSFg => "Intel XeSS 3 (SDK 3.0.2)",
      Self::NvidiaDlssFg => "Nvidia DLSS 4.5 (Streamline 2.14)",
    }
  }

  pub fn subtitle(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::Passthrough => tr(
        lang,
        "Direkte Low-Latency-DX12-Ausgabe inkl. Limited-RGB, Kontrast-Schärfe (CAS) & F4-Debug",
        "Direct low-latency DX12 output with Limited-RGB expansion, CAS sharpening & F4 debug",
      ),
      Self::AmdFsr3 => tr(
        lang,
        "AMD FidelityFX SDK 2.4.0: FSR FG 4.0 ML (Radeon) & universelles FG 3.1.6 Compute (2x)",
        "AMD FidelityFX SDK 2.4.0: FSR FG 4.0 ML (Radeon) & universal FG 3.1.6 compute (2x)",
      ),
      Self::IntelXeSSFg => tr(
        lang,
        "Intel XeSS 3.0.2: XMX- & DP4a-KI-Rekonstruktion, XeLL & Multi-Frame Generation (2x / 3x / 4x)",
        "Intel XeSS 3.0.2: XMX & DP4a AI reconstruction, XeLL & Multi-Frame Generation (2x / 3x / 4x)",
      ),
      Self::NvidiaDlssFg => tr(
        lang,
        "Nvidia Streamline 2.14.16: Transformer-Architektur, Reflex & Multi-Frame Generation (2x / 3x / 4x, inkl. DLSS-5-Check)",
        "Nvidia Streamline 2.14.16: Transformer model, Reflex & Multi-Frame Generation (2x / 3x / 4x, incl. DLSS 5 check)",
      ),
    }
  }

  pub fn hud_badge(self) -> &'static str {
    match self {
      Self::Passthrough => "Passthru",
      Self::AmdFsr3 => "FSR 4.1",
      Self::IntelXeSSFg => "XeSS 3",
      Self::NvidiaDlssFg => "DLSS 4.5",
    }
  }

  pub fn card_badge(self) -> Option<&'static str> {
    match self {
      Self::Passthrough => None,
      Self::AmdFsr3 => Some("2x ML / CS"),
      Self::IntelXeSSFg => Some("2x / 3x / 4x MFG"),
      Self::NvidiaDlssFg => Some("2x / 3x / 4x MFG"),
    }
  }

  pub fn accent_color(self) -> Color32 {
    match self {
      Self::Passthrough => rgb(148, 163, 184),
      Self::AmdFsr3 => rgb(239, 68, 68),
      Self::IntelXeSSFg => rgb(56, 189, 248),
      Self::NvidiaDlssFg => rgb(52, 211, 153),
    }
  }

  pub fn is_fg_backend(self) -> bool {
    !matches!(self, Self::Passthrough)
  }

  pub fn supports_multi_fg(self) -> bool {
    matches!(self, Self::IntelXeSSFg | Self::NvidiaDlssFg)
  }

  pub fn config_key(self) -> &'static str {
    match self {
      Self::Passthrough => "passthrough",
      Self::AmdFsr3 => "fsr",
      Self::IntelXeSSFg => "xess",
      Self::NvidiaDlssFg => "dlss",
    }
  }

  pub fn from_config_key(key: &str) -> Self {
    match key.trim().to_ascii_lowercase().as_str() {
      "fsr" | "fsr3" | "fsr4" | "amd" => Self::AmdFsr3,
      "xess" | "xess3" | "intel" => Self::IntelXeSSFg,
      "dlss" | "dlss45" | "nvidia" | "streamline" => Self::NvidiaDlssFg,
      _ => Self::Passthrough,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugViewMode {
  Normal,
  MotionVectors,
  EdgeAegisDepth,
  SplitScreen3Way,
}

impl DebugViewMode {
  pub const ALL: [Self; 4] = [
    Self::Normal,
    Self::MotionVectors,
    Self::EdgeAegisDepth,
    Self::SplitScreen3Way,
  ];

  pub fn label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::Normal => tr(lang, "Live-Video (Normal)", "Live Video (Normal)"),
      Self::MotionVectors => tr(lang, "Motion Vectors (Farbrad)", "Motion Vectors (Color Wheel)"),
      Self::EdgeAegisDepth => tr(lang, "Pseudo-Depth (Heatmap)", "Pseudo-Depth (Heatmap)"),
      Self::SplitScreen3Way => tr(lang, "3-fach Split-Screen", "3-Way Split Screen"),
    }
  }

  pub fn quick_label(self, lang: AppLanguage) -> &'static str {
    match self {
      Self::Normal => tr(lang, "Live-Video", "Live Video"),
      Self::MotionVectors => "Motion Vectors",
      Self::EdgeAegisDepth => "Pseudo-Depth",
      Self::SplitScreen3Way => tr(lang, "Split-Screen", "Split Screen"),
    }
  }

  pub fn short_badge(self) -> &'static str {
    match self {
      Self::Normal => "Normal",
      Self::MotionVectors => "MOTION",
      Self::EdgeAegisDepth => "DEPTH",
      Self::SplitScreen3Way => "3x-SPLIT",
    }
  }

  pub fn shader_index(self) -> u32 {
    match self {
      Self::Normal => 0,
      Self::MotionVectors => 1,
      Self::EdgeAegisDepth => 2,
      Self::SplitScreen3Way => 3,
    }
  }

  pub fn cycle_next(self) -> Self {
    match self {
      Self::Normal => Self::MotionVectors,
      Self::MotionVectors => Self::EdgeAegisDepth,
      Self::EdgeAegisDepth => Self::SplitScreen3Way,
      Self::SplitScreen3Way => Self::Normal,
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureHudStats<'a> {
  pub has_signal: bool,
  pub raw_fps: f32,
  pub unique_fps: f32,
  pub duplicate_ratio_pct: f32,
  pub resolution: Option<(u32, u32)>,
  pub format_label: &'a str,
  pub dropped_duplicates: u64,
}

#[derive(Clone, Debug)]
pub struct FgHudStats {
  pub fg_gpu_ms: f32,
  pub status_label: String,
  pub native_sdk: bool,
  pub active_multiplier: u32,
  pub effective_video_fps: f32,
  pub is_interpolated_phase: bool,
}

/// Laufzeit-Zustand der Benutzeroberflaeche inklusive zweisprachiger Darstellung,
/// D3D12-FPS-Limiter, Geraetelisten und Synchronisation mit `crate::config::AppSettings`.
pub struct UiState {
  pub language: AppLanguage,
  pub active_tab: SidebarTab,
  pub capture_devices: Vec<CaptureDeviceInfo>,
  pub selected_capture_idx: Option<usize>,
  pub device_scan_count: u32,
  pub target_resolution: TargetResolution,
  pub input_fps: InputFps,
  pub expand_limited_rgb: bool,
  pub duplicate_filter_60_to_30: bool,
  pub audio_input_devices: Vec<AudioDeviceInfo>,
  pub audio_output_devices: Vec<AudioDeviceInfo>,
  pub selected_audio_input_idx: Option<usize>,
  pub selected_audio_output_idx: Option<usize>,
  pub audio_scan_count: u32,
  pub audio_enabled: bool,
  pub audio_muted: bool,
  pub audio_volume_percent: f32,
  pub gbuffer_enabled: bool,
  pub debug_view_mode: DebugViewMode,
  pub motion_sensitivity_pct: f32,
  pub edge_aegis_strength_pct: f32,
  pub upscaler_backend: UpscalerBackend,
  pub frame_generation_enabled: bool,
  pub fg_multiplier: u32,
  pub half_res_flow_scale: bool,
  pub fast_gbuffer_search: bool,
  pub force_fast_compute_fg: bool,
  pub hud_protection: bool,
  pub cas_sharpness: f32,
  pub allow_tearing: bool,
  pub target_output_fps_limit: u32,
  pub menu_visible: bool,
  pub logo_texture: Option<TextureHandle>,
  pub settings_is_portable: bool,
  pub settings_path_display: String,
  preferred_video_device_name: Option<String>,
  preferred_audio_input_name: Option<String>,
  preferred_audio_output_name: Option<String>,
  video_explicitly_disabled: bool,
  audio_input_explicitly_disabled: bool,
}

impl Default for UiState {
  fn default() -> Self {
    Self {
      language: AppLanguage::default(),
      active_tab: SidebarTab::SignalAndAudio,
      capture_devices: Vec::new(),
      selected_capture_idx: None,
      device_scan_count: 0,
      target_resolution: TargetResolution::Auto,
      input_fps: InputFps::Fps60,
      expand_limited_rgb: true,
      duplicate_filter_60_to_30: false,
      audio_input_devices: Vec::new(),
      audio_output_devices: Vec::new(),
      selected_audio_input_idx: None,
      selected_audio_output_idx: None,
      audio_scan_count: 0,
      audio_enabled: true,
      audio_muted: false,
      audio_volume_percent: 100.0_f32,
      gbuffer_enabled: true,
      debug_view_mode: DebugViewMode::Normal,
      motion_sensitivity_pct: 100.0_f32,
      edge_aegis_strength_pct: 100.0_f32,
      upscaler_backend: UpscalerBackend::Passthrough,
      frame_generation_enabled: true,
      fg_multiplier: 2,
      half_res_flow_scale: false,
      fast_gbuffer_search: false,
      force_fast_compute_fg: false,
      hud_protection: false,
      cas_sharpness: 55.0_f32,
      allow_tearing: false,
      target_output_fps_limit: 0,
      menu_visible: true,
      logo_texture: None,
      settings_is_portable: true,
      settings_path_display: String::from("frameforge_settings.json"),
      preferred_video_device_name: None,
      preferred_audio_input_name: None,
      preferred_audio_output_name: None,
      video_explicitly_disabled: false,
      audio_input_explicitly_disabled: false,
    }
  }
}

impl UiState {
  /// Uebertraegt alle geladenen Werte aus `crate::config::AppSettings` in den UI-State
  /// sowie die OSD-Konfiguration und gleicht gespeicherte Geraetenamen intelligent ab.
  pub fn apply_settings(
    &mut self,
    settings: &crate::config::AppSettings,
    osd_config: &mut OsdConfig,
  ) {
    let clean = settings.clone().sanitized();

    self.language = clean.language;
    self.target_resolution = TargetResolution::from_config_key(&clean.resolution_preset);
    self.input_fps = InputFps::from_u32(clean.target_fps);
    self.expand_limited_rgb = clean.limited_rgb;
    self.duplicate_filter_60_to_30 = clean.dedup_60_to_30;

    self.audio_enabled = clean.audio_enabled;
    self.audio_muted = clean.audio_muted;
    self.audio_volume_percent = clean.audio_volume;

    self.upscaler_backend = UpscalerBackend::from_config_key(&clean.backend);
    self.frame_generation_enabled = clean.fg_enabled;
    self.fg_multiplier = clean.fg_multiplier.clamp(2, 4);
    self.cas_sharpness = clean.cas_sharpness;
    self.allow_tearing = clean.allow_tearing;
    self.target_output_fps_limit = if FPS_LIMIT_VALUES.contains(&clean.target_output_fps_limit) {
      clean.target_output_fps_limit
    } else {
      0
    };

    self.half_res_flow_scale = clean.half_res_flow_scale;
    self.fast_gbuffer_search = clean.fast_gbuffer_search;
    self.force_fast_compute_fg = clean.force_fast_compute_fg;
    self.hud_protection = clean.hud_protection;

    self.gbuffer_enabled = clean.gbuffer_enabled;
    self.motion_sensitivity_pct = clean.motion_sensitivity;
    self.edge_aegis_strength_pct = clean.edge_aegis_strength;

    self.preferred_video_device_name = clean.video_device_name.clone();
    self.preferred_audio_input_name = clean.audio_input_name.clone();
    self.preferred_audio_output_name = clean.audio_output_name.clone();
    self.video_explicitly_disabled = false;
    self.audio_input_explicitly_disabled = false;

    // Falls Geraetelisten bereits gescannt wurden (z. B. nach "Standardwerte laden"), sofort neu zuordnen:
    if !self.capture_devices.is_empty() {
      self.selected_capture_idx = self
        .preferred_video_device_name
        .as_deref()
        .and_then(|pref| find_matching_video_device(&self.capture_devices, pref))
        .or_else(|| {
          self
            .capture_devices
            .iter()
            .position(|d| d.looks_like_hdmi_capture())
            .or(Some(0))
        });
    }
    if !self.audio_input_devices.is_empty() {
      self.selected_audio_input_idx = self
        .preferred_audio_input_name
        .as_deref()
        .and_then(|pref| find_matching_audio_device(&self.audio_input_devices, pref))
        .or_else(|| find_preferred_capture_audio_index(&self.audio_input_devices));
    }
    if !self.audio_output_devices.is_empty() {
      self.selected_audio_output_idx = self
        .preferred_audio_output_name
        .as_deref()
        .and_then(|pref| find_matching_audio_device(&self.audio_output_devices, pref));
    }

    osd_config.enabled = clean.osd_enabled;
    osd_config.preset = OsdPreset::from_config_key(&clean.osd_preset);
    osd_config.pos_norm = clean.osd_pos_norm;
    osd_config.locked = clean.osd_locked;
    osd_config.hide_in_menu = clean.osd_hide_in_menu;
    osd_config.color_theme = OsdColorTheme::from_config_key(&clean.osd_color_scheme);
    osd_config.scale_percent = clean.osd_scale;
    osd_config.background_opacity_percent = clean.osd_bg_opacity;
    osd_config.box_width = clean.osd_box_width;
    osd_config.refresh_interval_ms = clean.osd_refresh_interval_ms;

    if osd_config.preset == OsdPreset::Custom {
      osd_config.rows = OsdRowFlags {
        gpu_and_vram: clean.osd_row_gpu_vram,
        cpu_usage: clean.osd_row_cpu,
        system_and_app_ram: clean.osd_row_ram,
        capture_signal: clean.osd_row_capture,
        gbuffer_compute: clean.osd_row_gbuffer,
        pipeline_backend: clean.osd_row_pipeline,
        audio_engine: clean.osd_row_audio,
        output_fps_frametime: clean.osd_row_output_fps,
        fps_percentiles: clean.osd_row_lows,
        frametime_graph: clean.osd_row_graph,
      };
    } else {
      osd_config.rows = OsdRowFlags::for_preset(osd_config.preset);
    }
  }

  /// Liest den kompletten aktuellen Zustand (inkl. Sprache `DE/EN`, Gerätenamen,
  /// `target_output_fps_limit` und per Maus gezogener `osd_pos_norm`) als `AppSettings` aus.
  pub fn export_settings(&self, osd_config: &OsdConfig) -> crate::config::AppSettings {
    let video_device_name = if self.video_explicitly_disabled {
      None
    } else {
      self
        .selected_capture_idx
        .and_then(|i| self.capture_devices.get(i))
        .map(|d| d.name.clone())
        .or_else(|| self.preferred_video_device_name.clone())
    };

    let audio_input_name = if self.audio_input_explicitly_disabled {
      None
    } else {
      self
        .selected_audio_input_idx
        .and_then(|i| self.audio_input_devices.get(i))
        .map(|d| d.raw_name.clone())
        .or_else(|| self.preferred_audio_input_name.clone())
    };

    let audio_output_name = if self.audio_scan_count == 0 && self.selected_audio_output_idx.is_none()
    {
      self.preferred_audio_output_name.clone()
    } else {
      self
        .selected_audio_output_idx
        .and_then(|i| self.audio_output_devices.get(i))
        .map(|d| d.raw_name.clone())
    };

    crate::config::AppSettings {
      language: self.language,

      video_device_name,
      resolution_preset: String::from(self.target_resolution.config_key()),
      target_fps: self.input_fps.as_u32(),
      limited_rgb: self.expand_limited_rgb,
      dedup_60_to_30: self.duplicate_filter_60_to_30,

      audio_enabled: self.audio_enabled,
      audio_muted: self.audio_muted,
      audio_volume: self.audio_volume_percent,
      audio_input_name,
      audio_output_name,

      backend: String::from(self.upscaler_backend.config_key()),
      fg_enabled: self.frame_generation_enabled,
      fg_multiplier: self.fg_multiplier.clamp(2, 4),
      cas_sharpness: self.cas_sharpness,
      allow_tearing: self.allow_tearing,
      target_output_fps_limit: self.target_output_fps_limit,

      half_res_flow_scale: self.half_res_flow_scale,
      fast_gbuffer_search: self.fast_gbuffer_search,
      force_fast_compute_fg: self.force_fast_compute_fg,
      hud_protection: self.hud_protection,

      gbuffer_enabled: self.gbuffer_enabled,
      motion_sensitivity: self.motion_sensitivity_pct,
      edge_aegis_strength: self.edge_aegis_strength_pct,

      osd_enabled: osd_config.enabled,
      osd_preset: String::from(osd_config.preset.config_key()),
      osd_pos_norm: osd_config.pos_norm,
      osd_locked: osd_config.locked,
      osd_hide_in_menu: osd_config.hide_in_menu,
      osd_color_scheme: String::from(osd_config.color_theme.config_key()),
      osd_scale: osd_config.scale_percent,
      osd_bg_opacity: osd_config.background_opacity_percent,
      osd_box_width: osd_config.box_width,
      osd_refresh_interval_ms: osd_config.refresh_interval_ms,
      osd_row_gpu_vram: osd_config.rows.gpu_and_vram,
      osd_row_cpu: osd_config.rows.cpu_usage,
      osd_row_ram: osd_config.rows.system_and_app_ram,
      osd_row_capture: osd_config.rows.capture_signal,
      osd_row_gbuffer: osd_config.rows.gbuffer_compute,
      osd_row_pipeline: osd_config.rows.pipeline_backend,
      osd_row_audio: osd_config.rows.audio_engine,
      osd_row_output_fps: osd_config.rows.output_fps_frametime,
      osd_row_lows: osd_config.rows.fps_percentiles,
      osd_row_graph: osd_config.rows.frametime_graph,
    }
    .sanitized()
  }

  pub fn apply_scanned_devices(&mut self, devices: Vec<CaptureDeviceInfo>) {
    let prev = self
      .selected_capture_idx
      .and_then(|i| self.capture_devices.get(i).cloned());
    self.device_scan_count = self.device_scan_count.saturating_add(1);
    self.capture_devices = devices;

    if self.capture_devices.is_empty() || self.video_explicitly_disabled {
      self.selected_capture_idx = None;
      return;
    }

    if let Some(p) = prev {
      if let Some(pos) = self
        .capture_devices
        .iter()
        .position(|d| d.index == p.index && d.name == p.name)
      {
        self.selected_capture_idx = Some(pos);
        return;
      }
    }

    if let Some(pref_name) = self.preferred_video_device_name.as_deref() {
      if let Some(pos) = find_matching_video_device(&self.capture_devices, pref_name) {
        self.selected_capture_idx = Some(pos);
        return;
      }
    }

    if self.selected_capture_idx.is_none() {
      self.selected_capture_idx = self
        .capture_devices
        .iter()
        .position(|d| d.looks_like_hdmi_capture())
        .or(Some(0));
    } else if let Some(idx) = self.selected_capture_idx {
      if idx >= self.capture_devices.len() {
        self.selected_capture_idx = Some(0);
      }
    }
  }

  pub fn apply_scanned_audio_devices(
    &mut self,
    inputs: Vec<AudioDeviceInfo>,
    outputs: Vec<AudioDeviceInfo>,
    logs: &mut LogStore,
  ) {
    let prev_in = self
      .selected_audio_input_idx
      .and_then(|i| self.audio_input_devices.get(i))
      .map(|d| d.raw_name.clone());
    let prev_out = self
      .selected_audio_output_idx
      .and_then(|i| self.audio_output_devices.get(i))
      .map(|d| d.raw_name.clone());
    let is_first = self.audio_scan_count == 0;
    let lang = self.language;

    self.audio_scan_count = self.audio_scan_count.saturating_add(1);
    self.audio_input_devices = inputs;
    self.audio_output_devices = outputs;

    // 1. Audio-Ausgabegeraet wiederherstellen (aktuelle Wahl -> gespeicherter Name -> System-Standard)
    self.selected_audio_output_idx = prev_out
      .as_deref()
      .and_then(|t| find_matching_audio_device(&self.audio_output_devices, t))
      .or_else(|| {
        self
          .preferred_audio_output_name
          .as_deref()
          .and_then(|pref| find_matching_audio_device(&self.audio_output_devices, pref))
      });

    // 2. Capture-Audio-Eingang wiederherstellen
    if self.audio_input_explicitly_disabled {
      self.selected_audio_input_idx = None;
      return;
    }

    if let Some(t) = prev_in.as_deref() {
      if let Some(pos) = find_matching_audio_device(&self.audio_input_devices, t) {
        self.selected_audio_input_idx = Some(pos);
        return;
      }
    }

    if let Some(pref_in) = self.preferred_audio_input_name.as_deref() {
      if let Some(pos) = find_matching_audio_device(&self.audio_input_devices, pref_in) {
        self.selected_audio_input_idx = Some(pos);
        if is_first {
          if let Some(dev) = self.audio_input_devices.get(pos) {
            logs.info(format!(
              "{}: '{}'",
              tr(
                lang,
                "[Audio] Gespeicherten Capture-Audio-Eingang wiederhergestellt",
                "[Audio] Restored saved capture audio input"
              ),
              dev.display_name
            ));
          }
        }
        return;
      }
    }

    if let Some(pref) = find_preferred_capture_audio_index(&self.audio_input_devices) {
      self.selected_audio_input_idx = Some(pref);
      if let Some(dev) = self.audio_input_devices.get(pref) {
        logs.info(format!(
          "{}: '{}'",
          tr(
            lang,
            "[Audio] Capture-Audio automatisch vorselektiert",
            "[Audio] Auto-selected capture audio input"
          ),
          dev.display_name
        ));
      }
    } else {
      self.selected_audio_input_idx = None;
      if is_first && !self.audio_input_devices.is_empty() {
        logs.info(tr(
          lang,
          "[Audio] Kein eindeutiges HDMI-Capture-Audiogerät erkannt (manuelle Auswahl möglich).",
          "[Audio] No dedicated HDMI capture audio device detected (manual selection available).",
        ));
      }
    }
  }

  pub fn desired_capture_config(&self) -> Option<CaptureConfig> {
    let dev = self.capture_devices.get(self.selected_capture_idx?)?;
    Some(CaptureConfig {
      device_index: dev.index.clone(),
      device_name: dev.display_label(),
      target_resolution: self.target_resolution,
      input_fps: self.input_fps,
    })
  }

  pub fn desired_audio_route(&self) -> Option<AudioRouteConfig> {
    let in_dev = self.audio_input_devices.get(self.selected_audio_input_idx?)?;
    let output_device_name = self
      .selected_audio_output_idx
      .and_then(|i| self.audio_output_devices.get(i))
      .map(|d| d.raw_name.clone());
    Some(AudioRouteConfig {
      input_device_name: in_dev.raw_name.clone(),
      output_device_name,
    })
  }

  pub fn audio_volume_factor(&self) -> f32 {
    (self.audio_volume_percent / 100.0_f32).clamp(0.0_f32, 2.0_f32)
  }

  pub fn configured_fg_multiplier(&self) -> u32 {
    if !self.upscaler_backend.is_fg_backend() || !self.frame_generation_enabled {
      1
    } else if self.upscaler_backend == UpscalerBackend::AmdFsr3 {
      2
    } else {
      self.fg_multiplier.clamp(2, 4)
    }
  }

  pub fn turbo_config(&self) -> PerformanceTurboConfig {
    PerformanceTurboConfig {
      half_res_flow_scale: self.half_res_flow_scale,
      fast_gbuffer_search: self.fast_gbuffer_search,
      force_fast_compute_fg: self.force_fast_compute_fg,
      hud_protection: self.hud_protection,
    }
  }

  pub fn active_turbo_count(&self) -> usize {
    usize::from(self.half_res_flow_scale)
      + usize::from(self.fast_gbuffer_search)
      + usize::from(self.force_fast_compute_fg)
      + usize::from(self.hud_protection)
  }

  pub fn gbuffer_params(&self) -> GBufferParams {
    GBufferParams {
      enabled: self.gbuffer_enabled,
      expand_limited_rgb: self.expand_limited_rgb,
      motion_sensitivity: (self.motion_sensitivity_pct / 100.0_f32).clamp(0.5_f32, 1.5_f32),
      edge_aegis_strength: (self.edge_aegis_strength_pct / 100.0_f32).clamp(0.0_f32, 2.0_f32),
      half_res_flow_scale: self.half_res_flow_scale,
      fast_search: self.fast_gbuffer_search,
    }
  }
}

fn find_matching_video_device(devices: &[CaptureDeviceInfo], target: &str) -> Option<usize> {
  let needle = target.trim();
  if needle.is_empty() {
    return None;
  }
  devices
    .iter()
    .position(|d| d.name == needle || d.display_label() == needle)
    .or_else(|| {
      let lower = needle.to_lowercase();
      devices.iter().position(|d| {
        d.name.to_lowercase() == lower
          || d.display_label().to_lowercase() == lower
          || d.name.to_lowercase().contains(&lower)
      })
    })
}

fn find_matching_audio_device(devices: &[AudioDeviceInfo], target: &str) -> Option<usize> {
  let needle = target.trim();
  if needle.is_empty() {
    return None;
  }
  devices
    .iter()
    .position(|d| d.raw_name == needle || d.display_name == needle)
    .or_else(|| {
      let lower = needle.to_lowercase();
      devices.iter().position(|d| {
        d.raw_name.to_lowercase() == lower
          || d.display_name.to_lowercase() == lower
          || d.display_name.to_lowercase().contains(&lower)
      })
    })
}

#[derive(Default, Debug, Clone, Copy)]
pub struct UiActions {
  pub toggle_fullscreen: bool,
  pub refresh_capture_devices: bool,
  pub refresh_audio_devices: bool,
  pub reset_requested: bool,
  pub osd_dragged: bool,
}

fn set_wstyle(
  w: &mut egui::style::WidgetVisuals,
  bg: Color32,
  weak: Color32,
  bs: Stroke,
  fs: Stroke,
) {
  w.bg_fill = bg;
  w.weak_bg_fill = weak;
  w.bg_stroke = bs;
  w.fg_stroke = fs;
  w.rounding = Rounding::same(8.0_f32);
}

pub fn configure_gaming_theme(ctx: &Context) {
  let mut style = (*ctx.style()).clone();
  style.spacing.item_spacing = Vec2::new(10.0_f32, 8.0_f32);
  style.spacing.button_padding = Vec2::new(11.0_f32, 6.0_f32);
  style.spacing.window_margin = Margin::same(14.0_f32);
  style.animation_time = 0.14_f32;

  let mut v = Visuals::dark();
  v.dark_mode = true;
  v.override_text_color = Some(rgb(230, 236, 245));
  v.window_fill = rgba(11, 16, 27, 244);
  v.panel_fill = rgba(11, 16, 27, 246);
  v.faint_bg_color = rgba(20, 30, 48, 200);
  v.extreme_bg_color = rgba(7, 11, 19, 248);
  v.window_rounding = Rounding::same(12.0_f32);
  v.menu_rounding = Rounding::same(9.0_f32);
  v.window_stroke = Stroke::new(1.0_f32, rgba(40, 59, 94, 210));

  set_wstyle(
    &mut v.widgets.noninteractive,
    rgba(18, 26, 42, 220),
    rgba(18, 26, 42, 205),
    Stroke::new(1.0_f32, rgba(40, 59, 94, 185)),
    Stroke::new(1.0_f32, rgb(203, 213, 225)),
  );
  set_wstyle(
    &mut v.widgets.inactive,
    rgba(24, 35, 56, 235),
    rgba(22, 32, 52, 225),
    Stroke::new(1.0_f32, rgba(52, 74, 112, 195)),
    Stroke::new(1.0_f32, rgb(226, 232, 240)),
  );
  set_wstyle(
    &mut v.widgets.hovered,
    rgba(33, 49, 78, 245),
    rgba(33, 49, 78, 240),
    Stroke::new(1.0_f32, rgb(56, 189, 248)),
    Stroke::new(1.5_f32, Color32::WHITE),
  );
  set_wstyle(
    &mut v.widgets.active,
    rgb(14, 165, 233),
    rgb(14, 165, 233),
    Stroke::new(1.0_f32, rgb(125, 211, 252)),
    Stroke::new(1.5_f32, Color32::WHITE),
  );

  v.selection.bg_fill = rgba(14, 165, 233, 195);
  v.selection.stroke = Stroke::new(1.0_f32, rgb(125, 211, 252));
  style.visuals = v;
  ctx.set_style(style);
}

#[allow(clippy::too_many_arguments)]
pub fn draw_ui(
  ctx: &Context,
  settings: &mut UiState,
  osd: &mut OsdConfig,
  logs: &mut LogStore,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  gbuffer_stats: GBufferStats,
  gbuffer_dims: (u32, u32),
  gbuffer_gpu_ms: f32,
  fg_stats: &FgHudStats,
  audio_telemetry: AudioLiveTelemetry,
  adapter_name: &str,
  tearing_supported: bool,
  swapchain_size: (u32, u32),
  is_fullscreen: bool,
) -> UiActions {
  let mut actions = UiActions::default();

  // Strikte egui-Panel-Reihenfolge:
  // TopBottomPanel::top -> TopBottomPanel::bottom -> SidePanel::left -> CentralPanel::default
  if settings.menu_visible {
    draw_top_bar(
      ctx,
      settings,
      osd,
      logs,
      telemetry,
      capture_stats,
      fg_stats,
      is_fullscreen,
      &mut actions,
    );
    draw_bottom_statusbar(
      ctx,
      settings,
      telemetry,
      capture_stats,
      gbuffer_stats,
      gbuffer_dims,
      gbuffer_gpu_ms,
      adapter_name,
    );
    draw_left_sidebar(ctx, settings, adapter_name, tearing_supported, swapchain_size);
    draw_central_workspace(
      ctx,
      settings,
      osd,
      logs,
      telemetry,
      capture_stats,
      gbuffer_stats,
      fg_stats,
      audio_telemetry,
      adapter_name,
      tearing_supported,
      swapchain_size,
      &mut actions,
    );
  }

  let effective_osd_mult = if !settings.upscaler_backend.is_fg_backend()
    || !settings.frame_generation_enabled
  {
    1
  } else if fg_stats.active_multiplier >= 2 {
    fg_stats.active_multiplier
  } else {
    settings.configured_fg_multiplier()
  };

  let osd_rt = OsdRuntimeContext {
    adapter_name,
    swapchain_size,
    allow_tearing_active: tearing_supported && settings.allow_tearing,
    capture_has_signal: capture_stats.has_signal,
    capture_resolution: capture_stats.resolution,
    capture_raw_fps: capture_stats.raw_fps,
    capture_unique_fps: capture_stats.unique_fps,
    capture_duplicate_ratio_pct: capture_stats.duplicate_ratio_pct,
    capture_format_label: capture_stats.format_label,
    expand_limited_rgb: settings.expand_limited_rgb,
    duplicate_filter_active: settings.duplicate_filter_60_to_30,
    gbuffer: gbuffer_stats,
    gbuffer_dims,
    gbuffer_gpu_ms,
    debug_view_mode: settings.debug_view_mode,
    backend: settings.upscaler_backend,
    frame_gen_enabled: settings.frame_generation_enabled,
    fg_multiplier: effective_osd_mult,
    fg_gpu_ms: fg_stats.fg_gpu_ms,
    fg_status_label: fg_stats.status_label.clone(),
    fg_native_sdk: fg_stats.native_sdk,
    effective_video_fps: fg_stats.effective_video_fps,
    is_interpolated_phase: fg_stats.is_interpolated_phase,
    cas_sharpness: settings.cas_sharpness,
    turbo: settings.turbo_config(),
    audio: audio_telemetry,
    menu_visible: settings.menu_visible,
  };
  actions.osd_dragged = draw_performance_osd(ctx, osd, telemetry, &osd_rt, settings.language);
  actions
}

#[allow(clippy::too_many_arguments)]
fn draw_top_bar(
  ctx: &Context,
  settings: &mut UiState,
  osd: &mut OsdConfig,
  logs: &mut LogStore,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  fg_stats: &FgHudStats,
  is_fullscreen: bool,
  actions: &mut UiActions,
) {
  let lang = settings.language;
  let frame = Frame::default()
    .fill(rgba(11, 16, 27, 248))
    .stroke(Stroke::new(1.0_f32, rgba(40, 59, 94, 210)))
    .inner_margin(Margin::symmetric(16.0_f32, 7.0_f32));

  TopBottomPanel::top("ff_top_bar")
    .exact_height(48.0_f32)
    .frame(frame)
    .show(ctx, |ui| {
      ui.horizontal(|ui| {
        let (logo_rect, _) = ui.allocate_exact_size(Vec2::new(42.0_f32, 34.0_f32), Sense::hover());
        draw_brand_logo(ctx, ui.painter(), logo_rect, &mut settings.logo_texture);
        ui.label(
          RichText::new("FRAMEFORGE")
            .font(FontId::proportional(17.5_f32))
            .strong()
            .color(rgb(248, 250, 252)),
        );

        Frame::default()
          .fill(rgba(14, 165, 233, 30))
          .stroke(Stroke::new(1.0_f32, rgba(56, 189, 248, 125)))
          .rounding(Rounding::same(5.0_f32))
          .inner_margin(Margin::symmetric(6.0_f32, 2.0_f32))
          .show(ui, |ui| {
            ui.label(
              RichText::new("v1.0 • DX12")
                .size(10.5_f32)
                .strong()
                .color(rgb(56, 189, 248)),
            );
          });
        ui.separator();

        // Beruhigte Snapshot-Statusanzeige (kein 500-Hz-Ziffernflimmern)
        let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(10.0_f32), Sense::hover());
        ui.painter().circle_filled(
          dot_rect.center(),
          4.0_f32,
          if capture_stats.has_signal {
            rgb(52, 211, 153)
          } else {
            rgb(100, 116, 139)
          },
        );
        let out_fps = telemetry.latched_d3d12_fps();
        let out_lbl = tr(lang, "Ausgabe", "Output");
        let fg_live = capture_stats.has_signal
          && settings.upscaler_backend.is_fg_backend()
          && settings.frame_generation_enabled;
        let status_line = if fg_live {
          let (w, h) = capture_stats.resolution.unwrap_or((0, 0));
          let mult = fg_stats
            .active_multiplier
            .max(settings.configured_fg_multiplier());
          let calm_eff = telemetry.latched_effective_video_fps();
          format!(
            "Signal {}x{} ({}, {:.1} -> {:.1}v FPS | {} {}x)  |  {}: {:.0} FPS",
            w,
            h,
            capture_stats.format_label,
            capture_stats.unique_fps,
            calm_eff,
            settings.upscaler_backend.hud_badge(),
            mult,
            out_lbl,
            out_fps
          )
        } else if capture_stats.has_signal {
          let (w, h) = capture_stats.resolution.unwrap_or((0, 0));
          format!(
            "Signal {}x{} ({}, {:.1} FPS)  |  {}: {:.0} FPS",
            w,
            h,
            capture_stats.format_label,
            capture_stats.unique_fps,
            out_lbl,
            out_fps
          )
        } else {
          format!("Standby  |  {}: {:.0} FPS", out_lbl, out_fps)
        };
        ui.label(
          RichText::new(status_line)
            .size(12.0_f32)
            .color(rgb(186, 198, 216)),
        );

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          ui.spacing_mut().item_spacing.x = 7.0_f32;

          let play_btn = Button::new(
            RichText::new(tr(lang, "Spielmodus (ESC)", "Game Mode (ESC)"))
              .size(12.0_f32)
              .strong()
              .color(rgb(11, 16, 27)),
          )
          .fill(rgb(245, 158, 11))
          .stroke(Stroke::new(1.0_f32, rgb(252, 211, 77)));
          if ui
            .add(play_btn)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
          {
            settings.menu_visible = false;
          }

          let fs_lbl = if is_fullscreen {
            tr(lang, "Fenstermodus (F11)", "Windowed (F11)")
          } else {
            tr(lang, "Vollbild (F11)", "Fullscreen (F11)")
          };
          if ui
            .add(Button::new(RichText::new(fs_lbl).size(12.0_f32)))
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
          {
            actions.toggle_fullscreen = true;
          }

          let osd_txt = if osd.enabled {
            tr(lang, "OSD: Ein (F2)", "OSD: On (F2)")
          } else {
            tr(lang, "OSD: Aus (F2)", "OSD: Off (F2)")
          };
          let osd_btn = if osd.enabled {
            Button::new(
              RichText::new(osd_txt)
                .size(12.0_f32)
                .color(rgb(252, 211, 77)),
            )
            .fill(rgba(245, 158, 11, 32))
            .stroke(Stroke::new(1.0_f32, rgba(245, 158, 11, 150)))
          } else {
            Button::new(
              RichText::new(osd_txt)
                .size(12.0_f32)
                .color(rgb(165, 180, 202)),
            )
          };
          if ui
            .add(osd_btn)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
          {
            osd.enabled = !osd.enabled;
            logs.info(format!(
              "{}: {}",
              tr(
                settings.language,
                "[OSD] Performance-Overlay umgeschaltet",
                "[OSD] Performance overlay toggled"
              ),
              if osd.enabled {
                tr(settings.language, "Ein", "On")
              } else {
                tr(settings.language, "Aus", "Off")
              }
            ));
          }

          ui.separator();

          // Kompakter Sprachumschalter (DE | EN) direkt links neben OSD / Vollbild / Spielmodus
          Frame::default()
            .fill(rgba(16, 24, 40, 235))
            .stroke(Stroke::new(1.0_f32, rgba(52, 74, 112, 195)))
            .rounding(Rounding::same(7.0_f32))
            .inner_margin(Margin::symmetric(4.0_f32, 2.0_f32))
            .show(ui, |ui| {
              ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0_f32;
                for lang_opt in AppLanguage::ALL {
                  let active = settings.language == lang_opt;
                  let btn = if active {
                    Button::new(
                      RichText::new(lang_opt.code())
                        .size(11.0_f32)
                        .strong()
                        .color(Color32::WHITE),
                    )
                    .fill(rgb(14, 165, 233))
                    .stroke(Stroke::new(1.0_f32, rgb(125, 211, 252)))
                    .rounding(Rounding::same(5.0_f32))
                  } else {
                    Button::new(
                      RichText::new(lang_opt.code())
                        .size(11.0_f32)
                        .color(rgb(165, 180, 202)),
                    )
                    .frame(false)
                  };
                  if ui
                    .add(btn)
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text(lang_opt.display_name())
                    .clicked()
                    && settings.language != lang_opt
                  {
                    settings.language = lang_opt;
                    logs.info(format!(
                      "{}: {}",
                      tr(
                        lang_opt,
                        "[UI] Sprache umgeschaltet auf",
                        "[UI] Language switched to"
                      ),
                      lang_opt.display_name()
                    ));
                  }
                }
              });
            });
        });
      });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_bottom_statusbar(
  ctx: &Context,
  settings: &mut UiState,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  gbuffer_stats: GBufferStats,
  gbuffer_dims: (u32, u32),
  gbuffer_gpu_ms: f32,
  adapter_name: &str,
) {
  let lang = settings.language;
  let frame = Frame::default()
    .fill(rgba(9, 14, 24, 248))
    .stroke(Stroke::new(1.0_f32, rgba(40, 59, 94, 205)))
    .inner_margin(Margin::symmetric(14.0_f32, 3.0_f32));

  TopBottomPanel::bottom("ff_bottom_statusbar")
    .exact_height(28.0_f32)
    .frame(frame)
    .show(ctx, |ui| {
      ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0_f32;
        let (icon_rect, _) = ui.allocate_exact_size(Vec2::new(22.0_f32, 18.0_f32), Sense::hover());
        draw_brand_logo(ctx, ui.painter(), icon_rect, &mut settings.logo_texture);
        ui.label(
          RichText::new("FrameForge v1.0 • by SprtnDio")
            .size(11.5_f32)
            .strong()
            .color(rgb(241, 245, 249)),
        );
        ui.separator();
        ui.label(
          RichText::new(shorten_gpu_name(adapter_name))
            .size(11.0_f32)
            .color(rgb(125, 211, 252)),
        );

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          ui.spacing_mut().item_spacing.x = 8.0_f32;
          ui.label(
            RichText::new(tr(
              lang,
              "Kurztasten: ESC Spielmodus • F2 OSD • F4 Debug • F11 Vollbild",
              "Hotkeys: ESC Game Mode • F2 OSD • F4 Debug • F11 Fullscreen",
            ))
            .size(11.0_f32)
            .color(rgb(148, 163, 184)),
          );
          ui.separator();

          let turbo_cnt = settings.active_turbo_count();
          let turbo_col = if turbo_cnt > 0 {
            rgb(110, 231, 183)
          } else {
            rgb(148, 163, 184)
          };
          let turbo_lbl = match lang {
            AppLanguage::De => format!("Turbo: {turbo_cnt}/4 aktiv"),
            AppLanguage::En => format!("Turbo: {turbo_cnt}/4 active"),
          };
          ui.label(
            RichText::new(turbo_lbl)
              .size(11.0_f32)
              .strong()
              .color(turbo_col),
          );
          ui.separator();

          let fg_forces_gb =
            settings.upscaler_backend.is_fg_backend() && settings.frame_generation_enabled;
          let (gb_w, gb_h) = if gbuffer_dims.0 > 0 && gbuffer_dims.1 > 0 {
            gbuffer_dims
          } else {
            (gbuffer_stats.width, gbuffer_stats.height)
          };
          let calm_gb_ms = telemetry.latched_gbuffer_gpu_ms();
          let gb_ms = if calm_gb_ms > 0.0_f32 {
            calm_gb_ms
          } else {
            gbuffer_gpu_ms
          };
          let (gb_txt, gb_col) = if !settings.gbuffer_enabled && !fg_forces_gb {
            (
              String::from(tr(
                lang,
                "G-Buffer: Aus (0.00 ms)",
                "G-Buffer: Off (0.00 ms)",
              )),
              rgb(148, 163, 184),
            )
          } else if capture_stats.has_signal && gbuffer_stats.active && gb_w > 0 && gb_h > 0 {
            (
              format!("G-Buffer: {gb_w}x{gb_h} ({gb_ms:.2} ms)"),
              rgb(125, 211, 252),
            )
          } else {
            (String::from("G-Buffer: Standby"), rgb(148, 163, 184))
          };
          ui.label(RichText::new(gb_txt).size(11.0_f32).color(gb_col));
        });
      });
    });
}

fn draw_left_sidebar(
  ctx: &Context,
  settings: &mut UiState,
  adapter_name: &str,
  tearing_supported: bool,
  swapchain_size: (u32, u32),
) {
  let lang = settings.language;
  let frame = Frame::default()
    .fill(rgba(10, 15, 25, 246))
    .stroke(Stroke::new(1.0_f32, rgba(40, 59, 94, 195)))
    .inner_margin(Margin::symmetric(12.0_f32, 14.0_f32));

  SidePanel::left("ff_left_sidebar")
    .exact_width(220.0_f32)
    .resizable(false)
    .frame(frame)
    .show(ctx, |ui| {
      ui.label(
        RichText::new(tr(lang, "NAVIGATION", "NAVIGATION"))
          .size(10.5_f32)
          .strong()
          .color(rgb(100, 116, 139)),
      );
      ui.add_space(4.0_f32);

      for tab in SidebarTab::ALL {
        let active = settings.active_tab == tab;
        let fill = if active {
          rgba(20, 34, 58, 245)
        } else {
          rgba(14, 21, 35, 120)
        };
        let border = if active {
          Stroke::new(1.0_f32, rgba(56, 189, 248, 180))
        } else {
          Stroke::new(1.0_f32, rgba(36, 52, 82, 95))
        };
        let resp = Frame::default()
          .fill(fill)
          .stroke(border)
          .rounding(Rounding::same(9.0_f32))
          .inner_margin(Margin::symmetric(12.0_f32, 8.5_f32))
          .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
              RichText::new(tab.title(lang))
                .size(13.5_f32)
                .strong()
                .color(if active {
                  Color32::WHITE
                } else {
                  rgb(214, 223, 237)
                }),
            );
            ui.label(
              RichText::new(tab.subtitle(lang))
                .size(11.0_f32)
                .color(if active {
                  rgb(125, 211, 252)
                } else {
                  rgb(125, 140, 165)
                }),
            );
          })
          .response
          .interact(Sense::click())
          .on_hover_cursor(CursorIcon::PointingHand);

        if active {
          let r = resp.rect;
          let bar = Rect::from_min_max(
            egui::pos2(r.left() + 2.5_f32, r.top() + 7.0_f32),
            egui::pos2(r.left() + 6.0_f32, r.bottom() - 7.0_f32),
          );
          ui.painter()
            .rect_filled(bar, Rounding::same(2.0_f32), rgb(245, 158, 11));
        }
        if resp.clicked() {
          settings.active_tab = tab;
        }
        ui.add_space(4.0_f32);
      }

      ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        let sync = if tearing_supported && settings.allow_tearing {
          tr(lang, "AllowTearing aktiv", "AllowTearing on")
        } else {
          tr(lang, "VSync aktiv", "VSync active")
        };
        ui.label(
          RichText::new(format!(
            "{}x{}  |  {}",
            swapchain_size.0, swapchain_size.1, sync
          ))
          .size(11.0_f32)
          .color(rgb(125, 140, 165)),
        );
        ui.label(
          RichText::new(shorten_gpu_name(adapter_name))
            .size(11.5_f32)
            .strong()
            .color(rgb(186, 198, 216)),
        );
        ui.separator();
      });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_central_workspace(
  ctx: &Context,
  settings: &mut UiState,
  osd: &mut OsdConfig,
  logs: &mut LogStore,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  gbuffer_stats: GBufferStats,
  fg_stats: &FgHudStats,
  audio: AudioLiveTelemetry,
  adapter_name: &str,
  tearing_supported: bool,
  swapchain_size: (u32, u32),
  actions: &mut UiActions,
) {
  let bg = if capture_stats.has_signal {
    rgba(8, 12, 20, 182)
  } else {
    rgba(11, 16, 27, 232)
  };
  CentralPanel::default()
    .frame(
      Frame::default()
        .fill(bg)
        .inner_margin(Margin::symmetric(22.0_f32, 18.0_f32)),
    )
    .show(ctx, |ui| {
      ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          ui.set_max_width(860.0_f32);
          match settings.active_tab {
            SidebarTab::SignalAndAudio => {
              draw_signal_and_audio_tab(ui, settings, telemetry, capture_stats, audio, actions)
            }
            SidebarTab::ImageAndFrameGen => draw_image_and_fg_tab(
              ui,
              settings,
              logs,
              telemetry,
              capture_stats,
              fg_stats,
              tearing_supported,
            ),
            SidebarTab::GBuffer3D => draw_gbuffer_tab(ui, settings, logs, telemetry, gbuffer_stats),
            SidebarTab::PerformanceOsd => draw_performance_osd_tab(ctx, ui, settings, osd, logs),
            SidebarTab::LogAndInfo => draw_log_and_info_tab(
              ctx,
              ui,
              settings,
              osd,
              logs,
              adapter_name,
              tearing_supported && settings.allow_tearing,
              swapchain_size,
              actions,
            ),
          }
        });
    });
}

fn section_card() -> Frame {
  Frame::default()
    .fill(rgba(15, 22, 36, 236))
    .stroke(Stroke::new(1.0_f32, rgba(44, 65, 102, 200)))
    .rounding(Rounding::same(12.0_f32))
    .inner_margin(Margin::same(16.0_f32))
}

fn section_header(ui: &mut Ui, title: &str, subtitle: &str, accent: Color32) {
  ui.label(
    RichText::new(title)
      .font(FontId::proportional(15.5_f32))
      .strong()
      .color(accent),
  );
  ui.label(
    RichText::new(subtitle)
      .size(12.0_f32)
      .color(rgb(148, 163, 184)),
  );
  ui.add_space(6.0_f32);
}

fn draw_signal_and_audio_tab(
  ui: &mut Ui,
  settings: &mut UiState,
  telemetry: &SystemTelemetry,
  stats: CaptureHudStats<'_>,
  audio: AudioLiveTelemetry,
  actions: &mut UiActions,
) {
  let lang = settings.language;

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "HDMI-Video-Quelle & Signalverarbeitung",
        "HDMI Video Source & Signal Processing",
      ),
      tr(
        lang,
        "Media-Foundation Zero-Backlog-Pipeline (unkomprimiertes NV12 / YUYV bevorzugt vor MJPEG)",
        "Media Foundation zero-backlog pipeline (uncompressed NV12 / YUYV prioritized over MJPEG)",
      ),
      rgb(56, 189, 248),
    );

    ui.horizontal(|ui| {
      let standby_lbl = tr(lang, "Standby / Deaktiviert", "Standby / Disabled");
      let prev_vid = settings.selected_capture_idx;
      let sel = settings
        .selected_capture_idx
        .and_then(|i| settings.capture_devices.get(i))
        .map(|d| d.display_label())
        .unwrap_or_else(|| String::from(standby_lbl));
      ComboBox::from_id_salt("capture_device_combo")
        .width((ui.available_width() - 135.0_f32).max(150.0_f32))
        .selected_text(sel)
        .show_ui(ui, |ui| {
          ui.selectable_value(&mut settings.selected_capture_idx, None, standby_lbl);
          for (i, d) in settings.capture_devices.iter().enumerate() {
            ui.selectable_value(&mut settings.selected_capture_idx, Some(i), d.display_label());
          }
        });
      if settings.selected_capture_idx != prev_vid {
        match settings.selected_capture_idx {
          None => {
            settings.video_explicitly_disabled = true;
            settings.preferred_video_device_name = None;
          }
          Some(idx) => {
            settings.video_explicitly_disabled = false;
            settings.preferred_video_device_name =
              settings.capture_devices.get(idx).map(|d| d.name.clone());
          }
        }
      }

      if ui
        .add(Button::new(tr(lang, "Neu scannen", "Rescan")))
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
      {
        actions.refresh_capture_devices = true;
      }
    });

    ui.add_space(4.0_f32);
    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        lbl12(ui, tr(lang, "Ziel-Auflösung", "Target Resolution"));
        enum_combo_lang(
          ui,
          "capture_res_combo",
          ui.available_width(),
          &mut settings.target_resolution,
          &TargetResolution::ALL,
          lang,
          TargetResolution::label,
        );
      });
      cols[1].vertical(|ui| {
        lbl12(ui, tr(lang, "Eingangs-FPS", "Input FPS"));
        enum_combo_lang(
          ui,
          "capture_fps_combo",
          ui.available_width(),
          &mut settings.input_fps,
          &InputFps::ALL,
          lang,
          InputFps::label,
        );
      });
    });

    ui.add_space(4.0_f32);
    ui.add(Checkbox::new(
      &mut settings.expand_limited_rgb,
      tr(
        lang,
        "HDMI Limited-RGB (16-235 -> 0-255) für Konsolen spreizen",
        "Expand HDMI Limited RGB (16-235 -> 0-255) for consoles",
      ),
    ));
    ui.add(Checkbox::new(
      &mut settings.duplicate_filter_60_to_30,
      tr(
        lang,
        "60 -> 30 FPS Duplikat-Filter (für 30-FPS-Spiele im 60-Hz-HDMI-Container)",
        "60 -> 30 FPS Duplicate Filter (for 30 FPS games inside a 60 Hz HDMI container)",
      ),
    ));

    let line = if stats.has_signal {
      let (w, h) = stats.resolution.unwrap_or((0, 0));
      match lang {
        AppLanguage::De => format!(
          "Signal aktiv: {}x{} ({})  |  Roh {:.1} FPS  |  Echte {:.1} FPS  |  Duplikate {:.0}% (verworfen: {})",
          w,
          h,
          stats.format_label,
          stats.raw_fps,
          stats.unique_fps,
          stats.duplicate_ratio_pct,
          stats.dropped_duplicates
        ),
        AppLanguage::En => format!(
          "Signal active: {}x{} ({})  |  Raw {:.1} FPS  |  Unique {:.1} FPS  |  Duplicates {:.0}% (dropped: {})",
          w,
          h,
          stats.format_label,
          stats.raw_fps,
          stats.unique_fps,
          stats.duplicate_ratio_pct,
          stats.dropped_duplicates
        ),
      }
    } else {
      match lang {
        AppLanguage::De => format!(
          "Standby: Kein aktives Videosignal (Geräte-Scans: {})",
          settings.device_scan_count
        ),
        AppLanguage::En => format!(
          "Standby: No active video signal (device scans: {})",
          settings.device_scan_count
        ),
      }
    };
    ui.label(
      RichText::new(line)
        .size(11.5_f32)
        .color(if stats.has_signal {
          rgb(110, 231, 183)
        } else {
          rgb(148, 163, 184)
        }),
    );
  });

  ui.add_space(14.0_f32);

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "HDMI-Audio-Passthrough (WASAPI Low-Latency)",
        "HDMI Audio Passthrough (WASAPI Low-Latency)",
      ),
      tr(
        lang,
        "Direkte Stereo-Bridge mit Linear-Resampler & automatischem Drift-Crossfade (~20 ms)",
        "Direct stereo bridge with linear resampler & automatic drift crossfade (~20 ms)",
      ),
      rgb(52, 211, 153),
    );

    ui.horizontal(|ui| {
      ui.add(Checkbox::new(
        &mut settings.audio_enabled,
        tr(lang, "Audio aktiv", "Enable Audio"),
      ));
      ui.add(Checkbox::new(
        &mut settings.audio_muted,
        tr(lang, "Stumm (Taste M)", "Mute (Key M)"),
      ));
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
          .add(Button::new(tr(lang, "Audio scannen", "Scan Audio")))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          actions.refresh_audio_devices = true;
        }
      });
    });

    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        let none_in_lbl = tr(
          lang,
          "Kein Capture-Audio gewählt",
          "No capture audio selected",
        );
        let prev_in = settings.selected_audio_input_idx;
        let in_lbl = settings
          .selected_audio_input_idx
          .and_then(|i| settings.audio_input_devices.get(i))
          .map(|d| d.display_name.clone())
          .unwrap_or_else(|| String::from(none_in_lbl));
        lbl12(ui, tr(lang, "Capture-Audio-Eingang", "Capture Audio Input"));
        ComboBox::from_id_salt("audio_in_combo")
          .width(ui.available_width())
          .selected_text(in_lbl)
          .show_ui(ui, |ui| {
            ui.selectable_value(
              &mut settings.selected_audio_input_idx,
              None,
              tr(lang, "Kein Eingangsgerät", "No Input Device"),
            );
            for (i, d) in settings.audio_input_devices.iter().enumerate() {
              ui.selectable_value(
                &mut settings.selected_audio_input_idx,
                Some(i),
                &d.display_name,
              );
            }
          });
        if settings.selected_audio_input_idx != prev_in {
          match settings.selected_audio_input_idx {
            None => {
              settings.audio_input_explicitly_disabled = true;
              settings.preferred_audio_input_name = None;
            }
            Some(idx) => {
              settings.audio_input_explicitly_disabled = false;
              settings.preferred_audio_input_name = settings
                .audio_input_devices
                .get(idx)
                .map(|d| d.raw_name.clone());
            }
          }
        }
      });

      cols[1].vertical(|ui| {
        let sys_default_lbl = tr(lang, "Windows System-Standard", "Windows System Default");
        let prev_out = settings.selected_audio_output_idx;
        let out_lbl = settings
          .selected_audio_output_idx
          .and_then(|i| settings.audio_output_devices.get(i))
          .map(|d| d.display_name.clone())
          .unwrap_or_else(|| String::from(sys_default_lbl));
        lbl12(
          ui,
          tr(
            lang,
            "Audio-Ausgabegerät (Headset / Lautsprecher)",
            "Audio Output Device (Headset / Speakers)",
          ),
        );
        ComboBox::from_id_salt("audio_out_combo")
          .width(ui.available_width())
          .selected_text(out_lbl)
          .show_ui(ui, |ui| {
            ui.selectable_value(
              &mut settings.selected_audio_output_idx,
              None,
              sys_default_lbl,
            );
            for (i, d) in settings.audio_output_devices.iter().enumerate() {
              ui.selectable_value(
                &mut settings.selected_audio_output_idx,
                Some(i),
                &d.display_name,
              );
            }
          });
        if settings.selected_audio_output_idx != prev_out {
          settings.preferred_audio_output_name = settings
            .selected_audio_output_idx
            .and_then(|idx| settings.audio_output_devices.get(idx))
            .map(|d| d.raw_name.clone());
        }
      });
    });

    lbl12(
      ui,
      tr(
        lang,
        "Lautstärke / Soft-Clip-Boost (0 % - 200 %)",
        "Volume / Soft-Clip Boost (0% - 200%)",
      ),
    );
    ui.add(
      Slider::new(&mut settings.audio_volume_percent, 0.0_f32..=200.0_f32)
        .suffix(" %")
        .step_by(1.0),
    );
    draw_stereo_meter(ui, "L", audio.current_peak_l);
    draw_stereo_meter(ui, "R", audio.current_peak_r);

    let info = if audio.active_stream_ok {
      match lang {
        AppLanguage::De => format!(
          "Stream aktiv: {} Hz  |  Puffer-Latenz: {:.1} ms",
          audio.sample_rate_hz,
          telemetry.latched_audio_latency_ms()
        ),
        AppLanguage::En => format!(
          "Stream active: {} Hz  |  Buffer latency: {:.1} ms",
          audio.sample_rate_hz,
          telemetry.latched_audio_latency_ms()
        ),
      }
    } else if !audio.enabled {
      String::from(tr(
        lang,
        "Audio-Engine pausiert",
        "Audio engine paused",
      ))
    } else {
      String::from(tr(
        lang,
        "Wartet auf aktiven Capture-Audiostream ...",
        "Waiting for active capture audio stream ...",
      ))
    };
    ui.label(RichText::new(info).size(11.5_f32).color(rgb(148, 163, 184)));
  });
}

fn draw_stereo_meter(ui: &mut Ui, ch: &str, level: f32) {
  ui.horizontal(|ui| {
    ui.label(RichText::new(ch).monospace().size(11.5_f32));
    let (rect, _) = ui.allocate_exact_size(
      Vec2::new((ui.available_width() - 8.0_f32).max(80.0_f32), 8.0_f32),
      Sense::hover(),
    );
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(3.0_f32), rgb(18, 26, 40));
    let fw = rect.width() * level.clamp(0.0_f32, 1.0_f32);
    if fw > 0.5_f32 {
      let col = if level > 0.88_f32 {
        rgb(251, 191, 36)
      } else {
        rgb(52, 211, 153)
      };
      p.rect_filled(
        Rect::from_min_size(rect.min, Vec2::new(fw, rect.height())),
        Rounding::same(3.0_f32),
        col,
      );
    }
  });
}

fn draw_image_and_fg_tab(
  ui: &mut Ui,
  settings: &mut UiState,
  logs: &mut LogStore,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  fg_stats: &FgHudStats,
  tearing_supported: bool,
) {
  let lang = settings.language;

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "Next-Gen Upscaling & Multi-Frame Generation (2x / 3x / 4x)",
        "Next-Gen Upscaling & Multi-Frame Generation (2x / 3x / 4x)",
      ),
      tr(
        lang,
        "Wähle das aktive DX12-Backend (Synthetic G-Buffer speist Motion Vectors & Pseudo-Depth direkt ein)",
        "Select the active DX12 backend (Synthetic G-Buffer feeds Motion Vectors & Pseudo-Depth directly)",
      ),
      rgb(245, 158, 11),
    );

    let prev_backend = settings.upscaler_backend;
    for b in UpscalerBackend::ALL {
      let sel = settings.upscaler_backend == b;
      let acc = b.accent_color();
      let fill = if sel {
        rgba(acc.r(), acc.g(), acc.b(), 34)
      } else {
        rgba(19, 28, 45, 210)
      };
      let stroke = if sel {
        Stroke::new(1.5_f32, acc)
      } else {
        Stroke::new(1.0_f32, rgba(48, 68, 102, 175))
      };
      let resp = Frame::default()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(9.0_f32))
        .inner_margin(Margin::symmetric(12.0_f32, 8.0_f32))
        .show(ui, |ui| {
          ui.set_min_width(ui.available_width());
          ui.horizontal(|ui| {
            ui.radio_value(
              &mut settings.upscaler_backend,
              b,
              RichText::new(b.label(lang)).strong(),
            );
            if let Some(badge_txt) = b.card_badge() {
              ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let badge_text_col = match b {
                  UpscalerBackend::AmdFsr3 => rgb(252, 165, 165),
                  UpscalerBackend::IntelXeSSFg => rgb(125, 211, 252),
                  UpscalerBackend::NvidiaDlssFg => rgb(110, 231, 183),
                  UpscalerBackend::Passthrough => rgb(186, 198, 216),
                };
                Frame::default()
                  .fill(rgba(acc.r(), acc.g(), acc.b(), 34))
                  .stroke(Stroke::new(1.0_f32, rgba(acc.r(), acc.g(), acc.b(), 155)))
                  .rounding(Rounding::same(5.0_f32))
                  .inner_margin(Margin::symmetric(6.0_f32, 1.5_f32))
                  .show(ui, |ui| {
                    ui.label(
                      RichText::new(badge_txt)
                        .size(10.5_f32)
                        .strong()
                        .color(badge_text_col),
                    );
                  });
              });
            }
          });
          ui.label(
            RichText::new(b.subtitle(lang))
              .size(11.5_f32)
              .color(rgb(165, 180, 202)),
          );
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
      if resp.clicked() {
        settings.upscaler_backend = b;
      }
      ui.add_space(4.0_f32);
    }

    ui.add_space(4.0_f32);
    let fg_toggle_enabled = settings.upscaler_backend.is_fg_backend();
    ui.add_enabled_ui(fg_toggle_enabled, |ui| {
      ui.add(Checkbox::new(
        &mut settings.frame_generation_enabled,
        tr(
          lang,
          "Zwischenbild-Generierung (Frame Generation) aktivieren",
          "Enable Frame Generation (Motion Interpolation)",
        ),
      ));
    });

    ui.add_space(6.0_f32);
    draw_multi_fg_selector(ui, settings, capture_stats);

    if settings.upscaler_backend.is_fg_backend()
      && settings.frame_generation_enabled
      && !settings.gbuffer_enabled
      && prev_backend != settings.upscaler_backend
    {
      settings.gbuffer_enabled = true;
      logs.info(match lang {
        AppLanguage::De => format!(
          "[GBuffer] Synthetic G-Buffer für {} automatisch aktiviert.",
          settings.upscaler_backend.label(lang)
        ),
        AppLanguage::En => format!(
          "[GBuffer] Automatically enabled Synthetic G-Buffer for {}.",
          settings.upscaler_backend.label(lang)
        ),
      });
    }

    if settings.upscaler_backend.is_fg_backend() {
      ui.add_space(8.0_f32);
      draw_fg_live_status_panel(ui, settings, telemetry, capture_stats, fg_stats);
    }
  });

  ui.add_space(14.0_f32);

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(
          RichText::new(tr(
            lang,
            "Performance-Turbo & Stabilisierung (Einzeln wählbar)",
            "Performance Turbo & Stabilization (Individually Selectable)",
          ))
          .font(FontId::proportional(15.5_f32))
          .strong()
          .color(rgb(52, 211, 153)),
        );
        ui.label(
          RichText::new(tr(
            lang,
            "Jede Option ist unabhängig per Häkchen de-/aktivierbar — optimal für integrierte GPUs (z. B. Intel Iris Xe)",
            "Each option can be toggled independently — ideal for integrated GPUs (e.g., Intel Iris Xe)",
          ))
          .size(12.0_f32)
          .color(rgb(148, 163, 184)),
        );
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let any_active = settings.active_turbo_count() > 0;
        if ui
          .add_enabled(any_active, Button::new(tr(lang, "Zurücksetzen", "Reset")))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          settings.half_res_flow_scale = false;
          settings.fast_gbuffer_search = false;
          settings.force_fast_compute_fg = false;
          settings.hud_protection = false;
        }
        let rec_btn = Button::new(
          RichText::new(tr(lang, "iGPU-Empfehlung setzen", "Apply iGPU Preset"))
            .size(12.0_f32)
            .strong()
            .color(rgb(11, 16, 27)),
        )
        .fill(rgb(52, 211, 153))
        .stroke(Stroke::new(1.0_f32, rgb(110, 231, 183)));
        if ui
          .add(rec_btn)
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          settings.half_res_flow_scale = true;
          settings.fast_gbuffer_search = true;
          settings.force_fast_compute_fg = true;
          settings.hud_protection = true;
        }
      });
    });

    ui.add_space(8.0_f32);
    ui.add(Checkbox::new(
      &mut settings.half_res_flow_scale,
      tr(
        lang,
        "G-Buffer Halbe Auflösung / Flow-Scale 50 % (z. B. 960x540 statt 1080p - spart ~75 % Last)",
        "Half-Resolution G-Buffer / 50% Flow Scale (e.g., 960x540 instead of 1080p - saves ~75% load)",
      ),
    ));
    ui.add(Checkbox::new(
      &mut settings.fast_gbuffer_search,
      tr(
        lang,
        "Schneller G-Buffer-Suchradius (Kompakter 5-Tap-Diamond - spart ~40 % Compute-Zeit)",
        "Fast G-Buffer Search Radius (Compact 5-tap diamond - saves ~40% compute time)",
      ),
    ));
    ui.add(Checkbox::new(
      &mut settings.force_fast_compute_fg,
      tr(
        lang,
        "iGPU-Leichtgewicht-Interpolator nutzen (Schneller Compute-Pfad statt schwerer KI-/SDK-DLLs)",
        "Use Lightweight iGPU Interpolator (Fast compute path instead of heavy AI/SDK DLLs)",
      ),
    ));
    ui.add(Checkbox::new(
      &mut settings.hud_protection,
      tr(
        lang,
        "Automatischer HUD-, Minimap- & Untertitel-Schutz (Scharfe Spiel-UI ohne Wabern)",
        "Automatic HUD, Minimap & Subtitle Protection (Crisp game UI without warping)",
      ),
    ));
  });

  ui.add_space(14.0_f32);

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "Bildschärfe, FPS-Limiter & Anzeige-Synchronisation",
        "Image Sharpness, FPS Limiter & Display Sync",
      ),
      tr(
        lang,
        "Kantentreue Kontrast-Nachschärfung, D3D12-Ausgabetaktung und flackerfreie DXGI-Ausgabe",
        "Edge-preserving contrast sharpening, D3D12 output pacing, and flicker-free DXGI presentation",
      ),
      rgb(56, 189, 248),
    );

    lbl12(
      ui,
      tr(
        lang,
        "Kontrast-Schärfe (CAS) (0 % - 100 %)",
        "Contrast Adaptive Sharpening (CAS) (0% - 100%)",
      ),
    );
    ui.add(
      Slider::new(&mut settings.cas_sharpness, 0.0_f32..=100.0_f32)
        .suffix(" %")
        .step_by(1.0),
    );

    ui.add_space(6.0_f32);
    lbl12(ui, tr(lang, "Ausgabe-FPS-Limit", "Output FPS Limit"));
    ui.horizontal_wrapped(|ui| {
      ComboBox::from_id_salt("output_fps_limit_combo")
        .width(250.0_f32)
        .selected_text(output_fps_limit_label(
          settings.target_output_fps_limit,
          lang,
        ))
        .show_ui(ui, |ui| {
          for &opt in &UI_FPS_LIMIT_OPTIONS {
            ui.selectable_value(
              &mut settings.target_output_fps_limit,
              opt,
              output_fps_limit_label(opt, lang),
            );
          }
        });

      let limit_desc = match settings.target_output_fps_limit {
        1 => tr(
          lang,
          "Koppelt die D3D12-Ausgabe automatisch an die effektive Video-/FG-Zielrate (z. B. 60 / 120 / 240 FPS).",
          "Automatically matches D3D12 presentation to the effective video/FG rate (e.g., 60 / 120 / 240 FPS).",
        ),
        0 => tr(
          lang,
          "Kein künstliches Software-Limit (bei aktivem VSync ruhig an die Monitor-Hz gekoppelt).",
          "No software FPS cap (cleanly locked to monitor refresh rate when VSync is active).",
        ),
        _ => tr(
          lang,
          "Feste FPS-Obergrenze verhindert unnötige 600+ Leerlauf-FPS und spart GPU-Energie.",
          "Fixed FPS cap prevents unnecessary 600+ idle FPS and reduces GPU power draw.",
        ),
      };
      ui.label(
        RichText::new(limit_desc)
          .size(11.5_f32)
          .color(rgb(148, 163, 184)),
      );
    });

    ui.add_space(6.0_f32);
    ui.add_enabled_ui(tearing_supported, |ui| {
      ui.add(Checkbox::new(
        &mut settings.allow_tearing,
        tr(
          lang,
          "DXGI AllowTearing / VRR (Minimale Eingabelatenz ohne VSync-Stau)",
          "DXGI AllowTearing / VRR (Minimum input latency without VSync queue)",
        ),
      ));
    });
    ui.label(
      RichText::new(tr(
        lang,
        "Tipp gegen VRR-Flackern: AllowTearing deaktivieren (sauberer Display-VSync) oder ein festes FPS-Limit wählen.",
        "Anti-flicker tip: Disable AllowTearing (clean display VSync) or set a fixed FPS cap.",
      ))
      .size(11.5_f32)
      .color(rgb(252, 211, 77)),
    );
  });
}

fn draw_multi_fg_selector(
  ui: &mut Ui,
  settings: &mut UiState,
  capture_stats: CaptureHudStats<'_>,
) {
  let lang = settings.language;
  let is_multi_capable = settings.upscaler_backend.supports_multi_fg();
  let interactive = is_multi_capable && settings.frame_generation_enabled;
  let displayed_mult = if settings.upscaler_backend == UpscalerBackend::AmdFsr3 {
    2
  } else {
    settings.fg_multiplier.clamp(2, 4)
  };
  let acc = settings.upscaler_backend.accent_color();

  Frame::default()
    .fill(rgba(11, 18, 30, 215))
    .stroke(Stroke::new(1.0_f32, rgba(44, 65, 102, 165)))
    .rounding(Rounding::same(8.0_f32))
    .inner_margin(Margin::symmetric(12.0_f32, 8.0_f32))
    .show(ui, |ui| {
      ui.set_min_width(ui.available_width());
      ui.horizontal_wrapped(|ui| {
        ui.label(
          RichText::new(tr(lang, "Multi-Frame-Faktor:", "Multi-Frame Factor:"))
            .size(12.0_f32)
            .strong()
            .color(rgb(214, 223, 237)),
        );

        ui.add_enabled_ui(interactive, |ui| {
          for (mult, btn_lbl) in [
            (2_u32, tr(lang, "2x (Standard)", "2x (Standard)")),
            (3_u32, "3x Multi-FG"),
            (4_u32, "4x Multi-FG"),
          ] {
            let selected = displayed_mult == mult && settings.upscaler_backend.is_fg_backend();
            let btn = if selected {
              Button::new(
                RichText::new(btn_lbl)
                  .size(12.0_f32)
                  .strong()
                  .color(Color32::WHITE),
              )
              .fill(rgba(acc.r(), acc.g(), acc.b(), 170))
              .stroke(Stroke::new(1.2_f32, acc))
            } else {
              Button::new(RichText::new(btn_lbl).size(12.0_f32))
            };
            if ui
              .add(btn)
              .on_hover_cursor(CursorIcon::PointingHand)
              .clicked()
            {
              settings.fg_multiplier = mult;
            }
          }
        });

        let hint = match settings.upscaler_backend {
          UpscalerBackend::Passthrough => String::from(tr(
            lang,
            "Wähle FSR 4.1, XeSS 3 oder DLSS 4.5 für Frame Generation",
            "Select FSR 4.1, XeSS 3, or DLSS 4.5 for Frame Generation",
          )),
          UpscalerBackend::AmdFsr3 => String::from(tr(
            lang,
            "AMD SDK nutzt festes 2x-Pacing (z. B. 30 -> 60 / 60 -> 120 FPS)",
            "AMD SDK uses fixed 2x pacing (e.g., 30 -> 60 / 60 -> 120 FPS)",
          )),
          UpscalerBackend::IntelXeSSFg | UpscalerBackend::NvidiaDlssFg => {
            if !settings.frame_generation_enabled {
              String::from(tr(
                lang,
                "Zwischenbild-Generierung pausiert (1x Direkt-Ausgabe)",
                "Frame Generation paused (1x direct output)",
              ))
            } else if capture_stats.has_signal && capture_stats.unique_fps > 0.5_f32 {
              match lang {
                AppLanguage::De => format!(
                  "Aktiv: {:.0} -> {:.0} FPS ({}x Taktung)",
                  capture_stats.unique_fps,
                  capture_stats.unique_fps * (displayed_mult as f32),
                  displayed_mult
                ),
                AppLanguage::En => format!(
                  "Active: {:.0} -> {:.0} FPS ({}x pacing)",
                  capture_stats.unique_fps,
                  capture_stats.unique_fps * (displayed_mult as f32),
                  displayed_mult
                ),
              }
            } else {
              let base_fps = settings.input_fps.as_u32();
              match lang {
                AppLanguage::De => format!(
                  "Ziel-Taktung: {} -> {} FPS ({}x Multi-FG)",
                  base_fps,
                  base_fps * displayed_mult,
                  displayed_mult
                ),
                AppLanguage::En => format!(
                  "Target pacing: {} -> {} FPS ({}x Multi-FG)",
                  base_fps,
                  base_fps * displayed_mult,
                  displayed_mult
                ),
              }
            }
          }
        };

        ui.label(
          RichText::new(hint)
            .size(11.5_f32)
            .color(if interactive {
              rgb(125, 211, 252)
            } else {
              rgb(148, 163, 184)
            }),
        );
      });
    });
}

fn draw_fg_live_status_panel(
  ui: &mut Ui,
  settings: &UiState,
  telemetry: &SystemTelemetry,
  capture_stats: CaptureHudStats<'_>,
  fg_stats: &FgHudStats,
) {
  let lang = settings.language;
  let effective_mult = if settings.upscaler_backend == UpscalerBackend::AmdFsr3 {
    2
  } else if fg_stats.active_multiplier >= 2 {
    fg_stats.active_multiplier
  } else {
    settings.fg_multiplier.clamp(2, 4)
  };

  let sdk_vendor_title = match settings.upscaler_backend {
    UpscalerBackend::AmdFsr3 => tr(
      lang,
      "Signiertes AMD FidelityFX SDK aktiv",
      "Signed AMD FidelityFX SDK active",
    ),
    UpscalerBackend::IntelXeSSFg => tr(
      lang,
      "Natives Intel XeSS 3 SDK aktiv",
      "Native Intel XeSS 3 SDK active",
    ),
    UpscalerBackend::NvidiaDlssFg => tr(
      lang,
      "Natives Nvidia Streamline / DLSS 4.5 SDK aktiv",
      "Native Nvidia Streamline / DLSS 4.5 SDK active",
    ),
    UpscalerBackend::Passthrough => tr(lang, "Direkt-Ausgabe", "Direct Output"),
  };

  let (dot_col, border_col, mode_text) = if !capture_stats.has_signal {
    let status_info = if fg_stats.status_label != "Inaktiv" && !fg_stats.status_label.is_empty() {
      match lang {
        AppLanguage::De => format!(
          "{} bereit ({}) — wartet auf aktives HDMI-Videosignal",
          settings.upscaler_backend.hud_badge(),
          fg_stats.status_label
        ),
        AppLanguage::En => format!(
          "{} ready ({}) — waiting for active HDMI video signal",
          settings.upscaler_backend.hud_badge(),
          fg_stats.status_label
        ),
      }
    } else {
      match lang {
        AppLanguage::De => format!(
          "{} bereit — wartet auf aktives HDMI-Videosignal",
          settings.upscaler_backend.label(lang)
        ),
        AppLanguage::En => format!(
          "{} ready — waiting for active HDMI video signal",
          settings.upscaler_backend.label(lang)
        ),
      }
    };
    (rgb(148, 163, 184), rgba(100, 116, 139, 130), status_info)
  } else if !settings.frame_generation_enabled {
    (
      rgb(251, 191, 36),
      rgba(251, 191, 36, 130),
      match lang {
        AppLanguage::De => format!(
          "{}: Frame Generation deaktiviert (1x Direkt-Ausgabe)",
          settings.upscaler_backend.hud_badge()
        ),
        AppLanguage::En => format!(
          "{}: Frame Generation disabled (1x direct output)",
          settings.upscaler_backend.hud_badge()
        ),
      },
    )
  } else if fg_stats.native_sdk {
    (
      rgb(52, 211, 153),
      rgba(52, 211, 153, 145),
      format!("{} ({})", sdk_vendor_title, fg_stats.status_label),
    )
  } else {
    (
      rgb(56, 189, 248),
      rgba(56, 189, 248, 145),
      match lang {
        AppLanguage::De => format!(
          "DX12 Multi-FG Compute-Engine aktiv ({})",
          fg_stats.status_label
        ),
        AppLanguage::En => format!(
          "DX12 Multi-FG compute engine active ({})",
          fg_stats.status_label
        ),
      },
    )
  };

  let calm_fg_ms = telemetry.latched_fg_gpu_ms();
  let calm_eff_fps = telemetry.latched_effective_video_fps();

  Frame::default()
    .fill(rgba(10, 16, 28, 228))
    .stroke(Stroke::new(1.0_f32, border_col))
    .rounding(Rounding::same(9.0_f32))
    .inner_margin(Margin::symmetric(14.0_f32, 10.0_f32))
    .show(ui, |ui| {
      ui.set_min_width(ui.available_width());
      ui.horizontal(|ui| {
        let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(10.0_f32), Sense::hover());
        ui.painter()
          .circle_filled(dot_rect.center(), 4.2_f32, dot_col);
        ui.label(
          RichText::new(mode_text)
            .size(12.5_f32)
            .strong()
            .color(dot_col),
        );
      });

      ui.add_space(6.0_f32);
      let rate_line = if capture_stats.has_signal && settings.frame_generation_enabled {
        match lang {
          AppLanguage::De => format!(
            "Effektive Video-Rate: {:.1} -> {:.1} FPS ({}x Monoton-Pacing)",
            capture_stats.unique_fps, calm_eff_fps, effective_mult
          ),
          AppLanguage::En => format!(
            "Effective video rate: {:.1} -> {:.1} FPS ({}x monotonic pacing)",
            capture_stats.unique_fps, calm_eff_fps, effective_mult
          ),
        }
      } else if capture_stats.has_signal {
        match lang {
          AppLanguage::De => format!(
            "Effektive Video-Rate: {:.1} FPS ({}x Monoton-Pacing pausiert)",
            capture_stats.unique_fps, effective_mult
          ),
          AppLanguage::En => format!(
            "Effective video rate: {:.1} FPS ({}x monotonic pacing paused)",
            capture_stats.unique_fps, effective_mult
          ),
        }
      } else {
        match lang {
          AppLanguage::De => format!(
            "Effektive Video-Rate: -- -> -- FPS ({}x Monoton-Pacing)",
            effective_mult
          ),
          AppLanguage::En => format!(
            "Effective video rate: -- -> -- FPS ({}x monotonic pacing)",
            effective_mult
          ),
        }
      };

      let phase_line = if capture_stats.has_signal && settings.frame_generation_enabled {
        let cadence_txt =
          if telemetry.has_verified_fg_cadence() || fg_stats.is_interpolated_phase {
            match effective_mult {
              4 => tr(
                lang,
                "4x Monoton-Taktung aktiv (3 Zwischenbilder + Echtbild N)",
                "4x monotonic cadence active (3 interpolated + real frame N)",
              ),
              3 => tr(
                lang,
                "3x Monoton-Taktung aktiv (2 Zwischenbilder + Echtbild N)",
                "3x monotonic cadence active (2 interpolated + real frame N)",
              ),
              _ => tr(
                lang,
                "2x Monoton-Taktung aktiv (N-0.5 + Echtbild N)",
                "2x monotonic cadence active (N-0.5 + real frame N)",
              ),
            }
          } else {
            tr(
              lang,
              "Initialisiere Historie (1. Frame)",
              "Initializing history (1st frame)",
            )
          };
        format!("{}: {cadence_txt}", tr(lang, "Kadenz-Status", "Cadence status"))
      } else if !settings.frame_generation_enabled {
        String::from(tr(
          lang,
          "Pausiert: 1x Originalbild-Taktung",
          "Paused: 1x native frame cadence",
        ))
      } else {
        String::from(tr(
          lang,
          "Standby: Wartet auf HDMI-Capture-Frames",
          "Standby: Waiting for HDMI capture frames",
        ))
      };

      let src_line = match (settings.half_res_flow_scale, settings.hud_protection) {
        (true, true) => tr(
          lang,
          "Quelle: G-Buffer Flow-Scale 50 % + HUD-/Untertitel-Schutz aktiv",
          "Source: G-Buffer 50% Flow Scale + HUD/Subtitle Protection active",
        ),
        (true, false) => tr(
          lang,
          "Quelle: G-Buffer Flow-Scale 50 % (Motion Vectors + Edge-Aegis Depth)",
          "Source: G-Buffer 50% Flow Scale (Motion Vectors + Edge-Aegis Depth)",
        ),
        (false, true) => tr(
          lang,
          "Quelle: G-Buffer 100 % + HUD-/Untertitel-Schutz aktiv",
          "Source: G-Buffer 100% + HUD/Subtitle Protection active",
        ),
        (false, false) => tr(
          lang,
          "Quelle: Synthetic G-Buffer (Motion Vectors + Edge-Aegis Depth)",
          "Source: Synthetic G-Buffer (Motion Vectors + Edge-Aegis Depth)",
        ),
      };

      ui.columns(2, |cols| {
        cols[0].vertical(|ui| {
          let gpu_lbl = match lang {
            AppLanguage::De => format!("FG GPU-Rechenzeit: {:.2} ms", calm_fg_ms),
            AppLanguage::En => format!("FG GPU compute time: {:.2} ms", calm_fg_ms),
          };
          ui.label(
            RichText::new(gpu_lbl)
              .monospace()
              .size(12.0_f32)
              .color(rgb(226, 232, 240)),
          );
          ui.label(
            RichText::new(phase_line)
              .size(11.5_f32)
              .color(rgb(165, 180, 202)),
          );
        });
        cols[1].vertical(|ui| {
          ui.label(
            RichText::new(rate_line)
              .monospace()
              .size(12.0_f32)
              .color(rgb(125, 211, 252)),
          );
          ui.label(
            RichText::new(src_line)
              .size(11.5_f32)
              .color(rgb(148, 163, 184)),
          );
        });
      });
    });
}

fn draw_gbuffer_tab(
  ui: &mut Ui,
  settings: &mut UiState,
  logs: &mut LogStore,
  telemetry: &SystemTelemetry,
  gbuffer_stats: GBufferStats,
) {
  let lang = settings.language;

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "Synthetic G-Buffer & Edge Aegis (3D-Rekonstruktion)",
        "Synthetic G-Buffer & Edge Aegis (3D Reconstruction)",
      ),
      tr(
        lang,
        "2-Pass DX12 Compute (cs_5_0): Block-Matching + Lucas-Kanade Motion Vectors & Pseudo-Depth",
        "2-pass DX12 Compute (cs_5_0): Block matching + Lucas-Kanade Motion Vectors & Pseudo-Depth",
      ),
      rgb(56, 189, 248),
    );

    ui.horizontal(|ui| {
      ui.add(Checkbox::new(
        &mut settings.gbuffer_enabled,
        tr(
          lang,
          "Synthetic G-Buffer aktivieren",
          "Enable Synthetic G-Buffer",
        ),
      ));
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
          .add(Button::new(tr(
            lang,
            "Standard (100 %)",
            "Default (100%)",
          )))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          settings.motion_sensitivity_pct = 100.0_f32;
          settings.edge_aegis_strength_pct = 100.0_f32;
        }
      });
    });

    ui.add_space(8.0_f32);
    lbl12(
      ui,
      tr(
        lang,
        "Live-Debug-Schnellwahl (Hotkey F4)",
        "Live Debug Quick Select (Hotkey F4)",
      ),
    );
    ui.columns(4, |cols| {
      for (idx, mode) in DebugViewMode::ALL.into_iter().enumerate() {
        let active = settings.debug_view_mode == mode;
        let btn = if active {
          Button::new(
            RichText::new(mode.quick_label(lang))
              .size(12.0_f32)
              .strong()
              .color(Color32::WHITE),
          )
          .fill(rgb(14, 165, 233))
          .stroke(Stroke::new(1.0_f32, rgb(125, 211, 252)))
        } else {
          Button::new(RichText::new(mode.quick_label(lang)).size(12.0_f32))
        };
        if cols[idx]
          .add_sized([cols[idx].available_width(), 30.0_f32], btn)
          .on_hover_cursor(CursorIcon::PointingHand)
          .on_hover_text(mode.label(lang))
          .clicked()
        {
          settings.debug_view_mode = mode;
          if mode != DebugViewMode::Normal && !settings.gbuffer_enabled {
            settings.gbuffer_enabled = true;
            logs.info(tr(
              lang,
              "[GBuffer] Synthetic G-Buffer für Debug-Ansicht automatisch aktiviert.",
              "[GBuffer] Automatically enabled Synthetic G-Buffer for debug view.",
            ));
          }
        }
      }
    });

    ui.add_space(10.0_f32);
    let fg_forces_gbuffer =
      settings.upscaler_backend.is_fg_backend() && settings.frame_generation_enabled;
    ui.add_enabled_ui(settings.gbuffer_enabled || fg_forces_gbuffer, |ui| {
      lbl12(
        ui,
        tr(
          lang,
          "Bewegungs-Empfindlichkeit / Suchreichweite (50 % - 150 %)",
          "Motion Sensitivity / Search Range (50% - 150%)",
        ),
      );
      ui.add(
        Slider::new(&mut settings.motion_sensitivity_pct, 50.0_f32..=150.0_f32)
          .suffix(" %")
          .step_by(1.0),
      );
      ui.add_space(4.0_f32);
      lbl12(
        ui,
        tr(
          lang,
          "Edge-Aegis Kantenschutz-Stärke (0 % - 200 %)",
          "Edge Aegis Protection Strength (0% - 200%)",
        ),
      );
      ui.add(
        Slider::new(&mut settings.edge_aegis_strength_pct, 0.0_f32..=200.0_f32)
          .suffix(" %")
          .step_by(1.0),
      );

      ui.add_space(6.0_f32);
      ui.separator();
      lbl12(
        ui,
        tr(
          lang,
          "G-Buffer Performance-Turbo (synchron mit Tab Bild & Frame Gen)",
          "G-Buffer Performance Turbo (synced with Display & Frame Gen tab)",
        ),
      );
      ui.add(Checkbox::new(
        &mut settings.half_res_flow_scale,
        tr(
          lang,
          "G-Buffer Halbe Auflösung / Flow-Scale 50 % (z. B. 960x540 statt 1080p - spart ~75 % Last)",
          "Half-Resolution G-Buffer / 50% Flow Scale (e.g., 960x540 instead of 1080p - saves ~75% load)",
        ),
      ));
      ui.add(Checkbox::new(
        &mut settings.fast_gbuffer_search,
        tr(
          lang,
          "Schneller G-Buffer-Suchradius (Kompakter 5-Tap-Diamond - spart ~40 % Compute-Zeit)",
          "Fast G-Buffer Search Radius (Compact 5-tap diamond - saves ~40% compute time)",
        ),
      ));
    });

    ui.add_space(8.0_f32);
    let calm_gb_ms = telemetry.latched_gbuffer_gpu_ms();
    let info = if !settings.gbuffer_enabled && !fg_forces_gbuffer {
      String::from(tr(
        lang,
        "GPU-Compute (GBUF): Deaktiviert (0.00 ms GPU-Last | Neutrale Fallback-SRV gebunden)",
        "GPU Compute (GBUF): Disabled (0.00 ms GPU load | Neutral fallback SRV bound)",
      ))
    } else if gbuffer_stats.active && gbuffer_stats.width > 0 {
      let scale_lbl = if gbuffer_stats.half_res_flow_scale {
        match lang {
          AppLanguage::De => format!(
            "Flow-Scale 50 % aus {}x{}",
            gbuffer_stats.video_width, gbuffer_stats.video_height
          ),
          AppLanguage::En => format!(
            "50% Flow Scale from {}x{}",
            gbuffer_stats.video_width, gbuffer_stats.video_height
          ),
        }
      } else {
        match lang {
          AppLanguage::De => format!(
            "Nativ 100 % ({}x{})",
            gbuffer_stats.video_width, gbuffer_stats.video_height
          ),
          AppLanguage::En => format!(
            "Native 100% ({}x{})",
            gbuffer_stats.video_width, gbuffer_stats.video_height
          ),
        }
      };
      let search_lbl = if gbuffer_stats.fast_search {
        "5-Tap-Diamond"
      } else {
        tr(lang, "Vollsuche + 3x3-LK", "Full Search + 3x3 LK")
      };
      format!(
        "GPU-Compute (GBUF): {:.2} ms  |  {}x{} ({scale_lbl}, {search_lbl})",
        calm_gb_ms, gbuffer_stats.width, gbuffer_stats.height
      )
    } else {
      String::from(tr(
        lang,
        "GPU-Compute (GBUF): Bereit (wartet auf HDMI-Capture-Frames)",
        "GPU Compute (GBUF): Standby (waiting for HDMI capture frames)",
      ))
    };
    Frame::default()
      .fill(rgba(10, 16, 28, 220))
      .stroke(Stroke::new(1.0_f32, rgba(56, 189, 248, 90)))
      .rounding(Rounding::same(8.0_f32))
      .inner_margin(Margin::symmetric(12.0_f32, 8.0_f32))
      .show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(info).size(12.0_f32).color(rgb(125, 211, 252)));
      });
  });
}

fn draw_performance_osd_tab(
  ctx: &Context,
  ui: &mut Ui,
  settings: &mut UiState,
  osd: &mut OsdConfig,
  logs: &mut LogStore,
) {
  let lang = settings.language;
  let screen_size = ctx.screen_rect().size();

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "Performance-Overlay (OSD) & Maus-Positionierung",
        "Performance Overlay (OSD) & Mouse Positioning",
      ),
      tr(
        lang,
        "Frei verschiebbares Echtzeit-Telemetrie-OSD mit Klick-Durchlass-Modus (Hotkeys F2 / F3)",
        "Freely draggable real-time telemetry OSD with click-through mode (Hotkeys F2 / F3)",
      ),
      rgb(245, 158, 11),
    );

    ui.add(Checkbox::new(
      &mut osd.enabled,
      tr(
        lang,
        "Performance-Overlay (OSD) einblenden (Hotkey F2)",
        "Show Performance Overlay (OSD) (Hotkey F2)",
      ),
    ));
    ui.add(Checkbox::new(
      &mut osd.locked,
      tr(
        lang,
        "OSD-Position sperren (Klick-Durchlass)",
        "Lock OSD Position (Click-Through)",
      ),
    ));
    ui.add(Checkbox::new(
      &mut osd.hide_in_menu,
      tr(
        lang,
        "OSD im Einstellungsmenü ausblenden (nur im Spielmodus sichtbar)",
        "Hide OSD while settings menu is open (visible in Game Mode only)",
      ),
    ));

    ui.add_space(6.0_f32);
    let drag_hint = if osd.locked {
      match lang {
        AppLanguage::De => format!(
          "Position gesperrt (Klick-Durchlass aktiv)  |  Normiert: {:.1} % / {:.1} %",
          osd.pos_norm[0] * 100.0_f32,
          osd.pos_norm[1] * 100.0_f32
        ),
        AppLanguage::En => format!(
          "Position locked (click-through active)  |  Normalized: {:.1}% / {:.1}%",
          osd.pos_norm[0] * 100.0_f32,
          osd.pos_norm[1] * 100.0_f32
        ),
      }
    } else {
      match lang {
        AppLanguage::De => format!(
          "Drag & Drop aktiv: Ziehe das Telemetrie-OSD direkt mit der Maus an jede beliebige Stelle ({:.1} % / {:.1} %).",
          osd.pos_norm[0] * 100.0_f32,
          osd.pos_norm[1] * 100.0_f32
        ),
        AppLanguage::En => format!(
          "Drag & Drop active: Drag the telemetry OSD anywhere on screen with your mouse ({:.1}% / {:.1}%).",
          osd.pos_norm[0] * 100.0_f32,
          osd.pos_norm[1] * 100.0_f32
        ),
      }
    };
    ui.label(
      RichText::new(drag_hint)
        .size(11.5_f32)
        .color(rgb(252, 211, 77)),
    );

    ui.add_space(6.0_f32);
    lbl12(
      ui,
      tr(
        lang,
        "Schnell-Positionen (anschließendes freies Ziehen bleibt aktiv)",
        "Quick Positions (free mouse dragging remains active afterwards)",
      ),
    );
    ui.horizontal_wrapped(|ui| {
      for slot in OsdQuickSlot::ALL {
        if ui
          .add(Button::new(slot.label(lang)))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          osd.apply_quick_slot(slot, screen_size);
        }
      }
    });
  });

  ui.add_space(14.0_f32);

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    section_header(
      ui,
      tr(
        lang,
        "Darstellung, Beruhigung & Sensoren",
        "Appearance, Smoothing & Sensors",
      ),
      tr(
        lang,
        "Aktualisierungs-Intervall gegen 500-Hz-Zahlenflimmern, Farbschema, Kastenbreite & Zeilen",
        "Refresh interval against 500 Hz digit flicker, color theme, fixed box width & sensor rows",
      ),
      rgb(56, 189, 248),
    );

    lbl12(
      ui,
      tr(
        lang,
        "Aktualisierungs-Intervall (Zahlen & Graph)",
        "Refresh Interval (Numbers & Graph)",
      ),
    );
    ui.columns(4, |cols| {
      for (idx, ms) in OSD_REFRESH_INTERVALS_MS.into_iter().enumerate() {
        let active = osd.refresh_interval_ms == ms;
        let lbl = osd_refresh_interval_label(ms, lang);
        let btn = if active {
          Button::new(
            RichText::new(lbl)
              .size(12.0_f32)
              .strong()
              .color(Color32::WHITE),
          )
          .fill(rgb(14, 165, 233))
          .stroke(Stroke::new(1.0_f32, rgb(125, 211, 252)))
        } else {
          Button::new(RichText::new(lbl).size(12.0_f32))
        };
        if cols[idx]
          .add_sized([cols[idx].available_width(), 30.0_f32], btn)
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
          && osd.refresh_interval_ms != ms
        {
          osd.refresh_interval_ms = ms;
          logs.info(match lang {
            AppLanguage::De => format!(
              "[OSD] Aktualisierungs-Intervall (Zahlen & Graph) gesetzt auf: {} ms.",
              ms
            ),
            AppLanguage::En => format!(
              "[OSD] Refresh interval (numbers & graph) set to: {} ms.",
              ms
            ),
          });
        }
      }
    });
    let bucket_ms = (osd.refresh_interval_ms.clamp(250, 2000) / 5).max(80);
    let throttle_info = match lang {
      AppLanguage::De => format!(
        "Snapshot-Drosselung aktiv: Zahlen alle {} ms gemittelt  |  Frametime-Graph geglättet in {}-ms-Zeit-Buckets (~{:.0} s Historie)",
        osd.refresh_interval_ms,
        bucket_ms,
        (bucket_ms as f32 * 150.0_f32) / 1000.0_f32
      ),
      AppLanguage::En => format!(
        "Snapshot smoothing active: Digits averaged every {} ms  |  Frametime graph smoothed in {} ms buckets (~{:.0} s history)",
        osd.refresh_interval_ms,
        bucket_ms,
        (bucket_ms as f32 * 150.0_f32) / 1000.0_f32
      ),
    };
    ui.label(
      RichText::new(throttle_info)
        .size(11.0_f32)
        .color(rgb(148, 163, 184)),
    );

    ui.add_space(6.0_f32);
    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        lbl12(ui, tr(lang, "OSD-Profil (Hotkey F3)", "OSD Preset (Hotkey F3)"));
        let mut sel = osd.preset;
        enum_combo_lang(
          ui,
          "osd_preset_combo",
          ui.available_width(),
          &mut sel,
          &OsdPreset::ALL,
          lang,
          OsdPreset::label,
        );
        if sel != osd.preset {
          osd.apply_preset(sel);
        }
      });
      cols[1].vertical(|ui| {
        lbl12(ui, tr(lang, "Farbschema", "Color Theme"));
        enum_combo_lang(
          ui,
          "osd_theme_combo",
          ui.available_width(),
          &mut osd.color_theme,
          &OsdColorTheme::ALL,
          lang,
          OsdColorTheme::label,
        );
      });
    });

    ui.add_space(4.0_f32);
    let box_lbl = tr(
      lang,
      "Kasten-Breite (Feste Größe)",
      "Box Width (Fixed Size)",
    );
    ui.horizontal(|ui| {
      lbl12(ui, box_lbl);
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
          .add(Button::new(tr(
            lang,
            "Standard (430 px)",
            "Default (430 px)",
          )))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          osd.box_width = 430.0_f32;
        }
      });
    });
    ui.add(
      Slider::new(&mut osd.box_width, 320.0_f32..=620.0_f32)
        .text(box_lbl)
        .suffix(" px")
        .step_by(5.0),
    );

    ui.add_space(4.0_f32);
    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        lbl12(ui, tr(lang, "Skalierung (75 % - 150 %)", "Scale (75% - 150%)"));
        ui.add(
          Slider::new(&mut osd.scale_percent, 75.0_f32..=150.0_f32)
            .suffix(" %")
            .step_by(5.0),
        );
      });
      cols[1].vertical(|ui| {
        lbl12(
          ui,
          tr(
            lang,
            "Hintergrund-Deckkraft (0 % - 90 %)",
            "Background Opacity (0% - 90%)",
          ),
        );
        ui.add(
          Slider::new(&mut osd.background_opacity_percent, 0.0_f32..=90.0_f32)
            .suffix(" %")
            .step_by(5.0),
        );
      });
    });

    ui.separator();
    lbl12(
      ui,
      tr(lang, "Sichtbare Telemetrie-Zeilen", "Visible Telemetry Rows"),
    );
    let mut ch = false;
    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        ch |= ui
          .checkbox(
            &mut osd.rows.gpu_and_vram,
            tr(lang, "GPU & VRAM (MEM)", "GPU & VRAM (MEM)"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.cpu_usage,
            tr(lang, "System-CPU (%)", "System CPU (%)"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.system_and_app_ram,
            tr(lang, "System- & App-RAM", "System & App RAM"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.capture_signal,
            tr(lang, "Capture-Signal (CAP)", "Capture Signal (CAP)"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.gbuffer_compute,
            "Synthetic G-Buffer (GBUF)",
          )
          .changed();
      });
      cols[1].vertical(|ui| {
        ch |= ui
          .checkbox(
            &mut osd.rows.pipeline_backend,
            tr(lang, "Backend & Multi-FG (PIPE)", "Backend & Multi-FG (PIPE)"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.audio_engine,
            tr(lang, "Audio-Engine (AUD)", "Audio Engine (AUD)"),
          )
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.output_fps_frametime,
            tr(lang, "Ausgabe-FPS (D3D12)", "Output FPS (D3D12)"),
          )
          .changed();
        ch |= ui
          .checkbox(&mut osd.rows.fps_percentiles, "1% / 0.1% Lows")
          .changed();
        ch |= ui
          .checkbox(
            &mut osd.rows.frametime_graph,
            tr(lang, "Frametime-Graph", "Frametime Graph"),
          )
          .changed();
      });
    });
    if ch {
      osd.preset = OsdPreset::Custom;
    }
  });
}

#[allow(clippy::too_many_arguments)]
fn draw_log_and_info_tab(
  ctx: &Context,
  ui: &mut Ui,
  settings: &mut UiState,
  osd: &mut OsdConfig,
  logs: &mut LogStore,
  adapter_name: &str,
  tearing_active: bool,
  swapchain_size: (u32, u32),
  actions: &mut UiActions,
) {
  let lang = settings.language;

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    ui.horizontal(|ui| {
      let (logo_rect, _) = ui.allocate_exact_size(Vec2::splat(88.0_f32), Sense::hover());
      draw_brand_logo(ctx, ui.painter(), logo_rect, &mut settings.logo_texture);
      ui.add_space(10.0_f32);
      ui.vertical(|ui| {
        ui.horizontal_wrapped(|ui| {
          ui.label(
            RichText::new("FrameForge v1.0.0")
              .font(FontId::proportional(20.0_f32))
              .strong()
              .color(Color32::WHITE),
          );
          Frame::default()
            .fill(rgba(245, 158, 11, 34))
            .stroke(Stroke::new(1.0_f32, rgba(252, 211, 77, 165)))
            .rounding(Rounding::same(5.0_f32))
            .inner_margin(Margin::symmetric(7.0_f32, 2.0_f32))
            .show(ui, |ui| {
              ui.label(
                RichText::new("Created by SprtnDio")
                  .size(11.5_f32)
                  .strong()
                  .color(rgb(252, 211, 77)),
              );
            });
        });

        ui.label(
          RichText::new(tr(
            lang,
            "Entwickelt von SprtnDio  •  DirectX 12 Capture, Synthetic G-Buffer & Multi-Vendor Frame Generation (FSR 4.1 / XeSS 3 / DLSS 4.5)",
            "Developed by SprtnDio  •  DirectX 12 Capture, Synthetic G-Buffer & Multi-Vendor Frame Generation (FSR 4.1 / XeSS 3 / DLSS 4.5)",
          ))
          .size(12.0_f32)
          .color(rgb(186, 198, 216)),
        );

        let sync_str = if tearing_active {
          "AllowTearing"
        } else {
          "VSync"
        };
        let hw_line = match lang {
          AppLanguage::De => format!(
            "Aktiver Adapter: {}  |  SwapChain: {}x{}  |  Sync: {}",
            adapter_name, swapchain_size.0, swapchain_size.1, sync_str
          ),
          AppLanguage::En => format!(
            "Active Adapter: {}  |  SwapChain: {}x{}  |  Sync: {}",
            adapter_name, swapchain_size.0, swapchain_size.1, sync_str
          ),
        };
        ui.label(
          RichText::new(hw_line)
            .size(11.5_f32)
            .color(rgb(125, 211, 252)),
        );

        let mode_badge = if settings.settings_is_portable {
          tr(lang, "Modus: Portable", "Mode: Portable")
        } else {
          tr(
            lang,
            "Modus: Installiert (%APPDATA%)",
            "Mode: Installed (%APPDATA%)",
          )
        };
        ui.label(
          RichText::new(format!(
            "{}  |  {}",
            mode_badge, settings.settings_path_display
          ))
          .monospace()
          .size(11.0_f32)
          .color(rgb(148, 163, 184)),
        );

        ui.add_space(6.0_f32);
        ui.horizontal_wrapped(|ui| {
          let reset_btn = Button::new(
            RichText::new(tr(lang, "Standardwerte laden", "Reset to Defaults"))
              .size(11.5_f32)
              .strong()
              .color(rgb(248, 250, 252)),
          )
          .fill(rgba(14, 165, 233, 42))
          .stroke(Stroke::new(1.0_f32, rgba(56, 189, 248, 165)));
          if ui
            .add(reset_btn)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
          {
            let keep_lang = settings.language;
            let defaults = crate::config::AppSettings {
              language: keep_lang,
              ..crate::config::AppSettings::default()
            };
            settings.apply_settings(&defaults, osd);
            actions.reset_requested = true;
            logs.info(tr(
              keep_lang,
              "[Config] Alle Einstellungen wurden auf die sicheren Standardwerte zurückgesetzt.",
              "[Config] All settings have been reset to safe factory defaults.",
            ));
          }

          if ui
            .add(
              Button::new(
                RichText::new(tr(
                  lang,
                  "Taskleisten-Icon-Cache auffrischen",
                  "Refresh Taskbar Icon Cache",
                ))
                .size(11.5_f32)
                .color(rgb(252, 211, 77)),
              )
              .fill(rgba(245, 158, 11, 28))
              .stroke(Stroke::new(1.0_f32, rgba(245, 158, 11, 145))),
            )
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
          {
            force_refresh_windows_icon_cache();
            logs.info(tr(
              lang,
              "[System] Windows-Taskleisten- & Explorer-Icon-Cache aufgefrischt (SHChangeNotify).",
              "[System] Refreshed Windows taskbar & Explorer icon cache (SHChangeNotify).",
            ));
          }
        });

        ui.add_space(4.0_f32);
        ui.label(
          RichText::new(tr(
            lang,
            "Unabhängiges Open-Source-Projekt von SprtnDio. Keine Verbindung zu AMD, Intel, NVIDIA oder Microsoft.",
            "Independent open-source project by SprtnDio. Not affiliated with AMD, Intel, NVIDIA, or Microsoft.",
          ))
          .size(11.0_f32)
          .color(rgb(125, 140, 165)),
        );
      });
    });

    ui.add_space(8.0_f32);
    ui.separator();
    lbl12(ui, tr(lang, "Globale Tastenkürzel", "Global Hotkeys"));
    ui.columns(2, |cols| {
      cols[0].vertical(|ui| {
        hotkey_row(
          ui,
          "ESC",
          tr(
            lang,
            "Einstellungsmenü / Spielmodus umschalten",
            "Toggle Settings Menu / Game Mode",
          ),
        );
        hotkey_row(
          ui,
          "F2",
          tr(
            lang,
            "Performance-Overlay (OSD) ein- / ausblenden",
            "Show / hide Performance Overlay (OSD)",
          ),
        );
        hotkey_row(
          ui,
          "F3",
          tr(
            lang,
            "OSD-Profil zyklisch durchschalten",
            "Cycle OSD preset",
          ),
        );
      });
      cols[1].vertical(|ui| {
        hotkey_row(
          ui,
          "F4",
          tr(
            lang,
            "3D-G-Buffer Live-Debug-Ansicht wechseln",
            "Cycle 3D G-Buffer live debug view",
          ),
        );
        hotkey_row(
          ui,
          "F11",
          tr(
            lang,
            "Randloses Vollbild / Fenstermodus",
            "Borderless Fullscreen / Windowed mode",
          ),
        );
        hotkey_row(
          ui,
          "M",
          tr(
            lang,
            "HDMI-Audio sofort stummschalten",
            "Mute HDMI audio immediately",
          ),
        );
      });
    });
  });

  ui.add_space(14.0_f32);

  section_card().show(ui, |ui| {
    ui.set_min_width(ui.available_width());
    ui.horizontal(|ui| {
      let console_title = match lang {
        AppLanguage::De => format!("System- & Pipeline-Konsole ({} Einträge)", logs.len()),
        AppLanguage::En => format!("System & Pipeline Console ({} entries)", logs.len()),
      };
      ui.label(
        RichText::new(console_title)
          .font(FontId::proportional(15.0_f32))
          .strong()
          .color(rgb(56, 189, 248)),
      );
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
          .add_enabled(!logs.is_empty(), Button::new(tr(lang, "Leeren", "Clear")))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          logs.clear();
        }
        if ui
          .add_enabled(!logs.is_empty(), Button::new(tr(lang, "Kopieren", "Copy")))
          .on_hover_cursor(CursorIcon::PointingHand)
          .clicked()
        {
          ctx.copy_text(logs.format_all_plain());
        }
      });
    });

    ui.add_space(6.0_f32);
    Frame::default()
      .fill(rgb(7, 11, 18))
      .stroke(Stroke::new(1.0_f32, rgb(35, 49, 74)))
      .rounding(Rounding::same(8.0_f32))
      .inner_margin(Margin::same(10.0_f32))
      .show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ScrollArea::vertical()
          .id_salt("ff_system_log_scroll")
          .max_height(260.0_f32)
          .stick_to_bottom(true)
          .show(ui, |ui| {
            for e in logs.entries() {
              ui.horizontal_wrapped(|ui| {
                ui.label(
                  RichText::new(format!("[+{:6.2}s]", e.timestamp_secs))
                    .monospace()
                    .size(11.0_f32)
                    .color(rgb(100, 116, 139)),
                );
                ui.label(
                  RichText::new(format!("[{}]", e.level.tag()))
                    .monospace()
                    .size(11.0_f32)
                    .color(e.level.color()),
                );
                ui.label(RichText::new(&e.message).monospace().size(11.5_f32));
              });
            }
          });
      });
  });
}

fn hotkey_row(ui: &mut Ui, key: &str, desc: &str) {
  ui.horizontal(|ui| {
    Frame::default()
      .fill(rgba(24, 36, 58, 235))
      .stroke(Stroke::new(1.0_f32, rgba(64, 92, 138, 195)))
      .rounding(Rounding::same(5.0_f32))
      .inner_margin(Margin::symmetric(6.0_f32, 2.0_f32))
      .show(ui, |ui| {
        ui.label(
          RichText::new(key)
            .monospace()
            .size(11.0_f32)
            .strong()
            .color(rgb(252, 211, 77)),
        );
      });
    ui.label(RichText::new(desc).size(11.5_f32).color(rgb(186, 198, 216)));
  });
}
