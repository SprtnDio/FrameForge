use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::i18n::AppLanguage;

const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(400);
const PORTABLE_MARKER_FILE: &str = "portable.mode";
const PORTABLE_SETTINGS_FILE: &str = "frameforge_settings.json";
const APPDATA_FOLDER_NAME: &str = "FrameForge";
const APPDATA_SETTINGS_FILE: &str = "settings.json";

/// Waehlbare Stufen fuer den D3D12-Ausgabe-FPS-Limiter:
/// `0` = Unbegrenzt / reiner VSync-Takt,
/// `1` = Auto (exakt passend zur effektiven Video-/FG-Zielrate, z. B. 60 / 120 / 240 FPS),
/// `60`, `120`, `144`, `240` = feste FPS-Obergrenzen gegen unnoetige 600+ Leerlauf-FPS.
pub const FPS_LIMIT_VALUES: [u32; 6] = [0, 1, 60, 120, 144, 240];

/// Vollstaendiges persistentes Einstellungs-Abbild fuer Portable-Modus (`frameforge_settings.json`)
/// und installierten Windows-`.msi`-Modus (`%APPDATA%\FrameForge\settings.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
  // Sprache
  pub language: AppLanguage,

  // Video-Capture
  pub video_device_name: Option<String>,
  pub resolution_preset: String,
  pub target_fps: u32,
  pub limited_rgb: bool,
  pub dedup_60_to_30: bool,

  // Audio (WASAPI)
  pub audio_enabled: bool,
  pub audio_muted: bool,
  pub audio_volume: f32,
  pub audio_input_name: Option<String>,
  pub audio_output_name: Option<String>,

  // Bild, Multi-Frame Generation & Anti-Flacker-Synchronisation
  pub backend: String,
  pub fg_enabled: bool,
  pub fg_multiplier: u32,
  pub cas_sharpness: f32,
  /// Standardmaessig `false` (= ruhiger, flackerfreier Display-VSync ohne VRR-Helligkeitsflackern).
  pub allow_tearing: bool,
  /// `0` = Unbegrenzt (bei aktivem VSync an Display-Hz gekoppelt), `1` = Auto (passend zur Video-FG-Rate),
  /// oder feste Obergrenzen `60` / `120` / `144` / `240` gegen 600+ Leerlauf-FPS.
  pub target_output_fps_limit: u32,

  // Performance-Turbo (4 einzeln schaltbare Optionen)
  pub half_res_flow_scale: bool,
  pub fast_gbuffer_search: bool,
  pub force_fast_compute_fg: bool,
  pub hud_protection: bool,

  // 3D-G-Buffer & Edge Aegis
  pub gbuffer_enabled: bool,
  pub motion_sensitivity: f32,
  pub edge_aegis_strength: f32,

  // Performance-OSD
  pub osd_enabled: bool,
  pub osd_preset: String,
  pub osd_pos_norm: [f32; 2],
  pub osd_locked: bool,
  pub osd_hide_in_menu: bool,
  pub osd_color_scheme: String,
  pub osd_scale: f32,
  pub osd_bg_opacity: f32,
  pub osd_box_width: f32,
  pub osd_refresh_interval_ms: u32,
  pub osd_row_gpu_vram: bool,
  pub osd_row_cpu: bool,
  pub osd_row_ram: bool,
  pub osd_row_capture: bool,
  pub osd_row_gbuffer: bool,
  pub osd_row_pipeline: bool,
  pub osd_row_audio: bool,
  pub osd_row_output_fps: bool,
  pub osd_row_lows: bool,
  pub osd_row_graph: bool,
}

impl Default for AppSettings {
  fn default() -> Self {
    Self {
      language: AppLanguage::default(),

      video_device_name: None,
      resolution_preset: String::from("auto"),
      target_fps: 60,
      limited_rgb: true,
      dedup_60_to_30: false,

      audio_enabled: true,
      audio_muted: false,
      audio_volume: 100.0_f32,
      audio_input_name: None,
      audio_output_name: None,

      backend: String::from("passthrough"),
      fg_enabled: true,
      fg_multiplier: 2,
      cas_sharpness: 55.0_f32,
      allow_tearing: false,
      target_output_fps_limit: 0,

      half_res_flow_scale: false,
      fast_gbuffer_search: false,
      force_fast_compute_fg: false,
      hud_protection: false,

      gbuffer_enabled: true,
      motion_sensitivity: 100.0_f32,
      edge_aegis_strength: 100.0_f32,

      osd_enabled: true,
      osd_preset: String::from("standard"),
      osd_pos_norm: [0.015_f32, 0.075_f32],
      osd_locked: false,
      osd_hide_in_menu: false,
      osd_color_scheme: String::from("amber"),
      osd_scale: 100.0_f32,
      osd_bg_opacity: 48.0_f32,
      osd_box_width: 430.0_f32,
      osd_refresh_interval_ms: 500,
      osd_row_gpu_vram: true,
      osd_row_cpu: true,
      osd_row_ram: true,
      osd_row_capture: true,
      osd_row_gbuffer: true,
      osd_row_pipeline: true,
      osd_row_audio: false,
      osd_row_output_fps: true,
      osd_row_lows: true,
      osd_row_graph: true,
    }
  }
}

impl AppSettings {
  /// Bereinigt und begrenzt alle geladenen JSON-Felder auf sichere, endliche Wertebereiche.
  pub fn sanitized(mut self) -> Self {
    self.resolution_preset = match self.resolution_preset.trim().to_ascii_lowercase().as_str() {
      "1080p" | "1920x1080" => String::from("1080p"),
      "720p" | "1280x720" => String::from("720p"),
      _ => String::from("auto"),
    };
    self.target_fps = if self.target_fps <= 30 { 30 } else { 60 };
    self.audio_volume = sanitize_f32(self.audio_volume, 100.0_f32, 0.0_f32, 200.0_f32);

    self.backend = match self.backend.trim().to_ascii_lowercase().as_str() {
      "fsr" | "fsr3" | "fsr4" | "amd" => String::from("fsr"),
      "xess" | "xess3" | "intel" => String::from("xess"),
      "dlss" | "dlss45" | "nvidia" | "streamline" => String::from("dlss"),
      _ => String::from("passthrough"),
    };
    self.fg_multiplier = self.fg_multiplier.clamp(2, 4);
    self.cas_sharpness = sanitize_f32(self.cas_sharpness, 55.0_f32, 0.0_f32, 100.0_f32);

    if !FPS_LIMIT_VALUES.contains(&self.target_output_fps_limit) {
      self.target_output_fps_limit = if self.allow_tearing { 120 } else { 0 };
    }

    self.motion_sensitivity =
      sanitize_f32(self.motion_sensitivity, 100.0_f32, 50.0_f32, 150.0_f32);
    self.edge_aegis_strength =
      sanitize_f32(self.edge_aegis_strength, 100.0_f32, 0.0_f32, 200.0_f32);

    self.osd_preset = match self.osd_preset.trim().to_ascii_lowercase().as_str() {
      "compact" | "one_line" => String::from("compact"),
      "full" | "diagnostics" => String::from("full"),
      "custom" => String::from("custom"),
      _ => String::from("standard"),
    };
    self.osd_pos_norm = [
      sanitize_f32(self.osd_pos_norm[0], 0.015_f32, 0.0_f32, 1.0_f32),
      sanitize_f32(self.osd_pos_norm[1], 0.075_f32, 0.0_f32, 1.0_f32),
    ];
    self.osd_color_scheme = match self.osd_color_scheme.trim().to_ascii_lowercase().as_str() {
      "cyan" => String::from("cyan"),
      "emerald" | "green" => String::from("emerald"),
      "mono" | "white" | "monochrome" => String::from("mono"),
      _ => String::from("amber"),
    };
    self.osd_scale = sanitize_f32(self.osd_scale, 100.0_f32, 75.0_f32, 150.0_f32);
    self.osd_bg_opacity = sanitize_f32(self.osd_bg_opacity, 48.0_f32, 0.0_f32, 90.0_f32);
    self.osd_box_width = sanitize_f32(self.osd_box_width, 430.0_f32, 320.0_f32, 620.0_f32);
    self.osd_refresh_interval_ms = match self.osd_refresh_interval_ms {
      0..=374 => 250,
      375..=749 => 500,
      750..=1499 => 1000,
      _ => 2000,
    };

    normalize_optional_name(&mut self.video_device_name);
    normalize_optional_name(&mut self.audio_input_name);
    normalize_optional_name(&mut self.audio_output_name);
    self
  }
}

#[inline]
fn sanitize_f32(val: f32, default: f32, min: f32, max: f32) -> f32 {
  if val.is_finite() {
    val.clamp(min, max)
  } else {
    default
  }
}

fn normalize_optional_name(slot: &mut Option<String>) {
  if let Some(s) = slot {
    let trimmed = s.trim();
    if trimmed.is_empty() {
      *slot = None;
    } else if trimmed.len() != s.len() {
      *slot = Some(trimmed.to_string());
    }
  }
}

/// Ermittelt automatisch den Speicherort fuer Einstellungen:
/// 1. Liegt neben `frameforge.exe` die Markierungsdatei `portable.mode` oder bereits
///    `frameforge_settings.json` -> Portable-Modus (`<exe_dir>/frameforge_settings.json`, `true`).
/// 2. Andernfalls (z. B. `.msi`-Installation unter `C:\Program Files\FrameForge`) ->
///    Benutzerprofil `%APPDATA%\FrameForge\settings.json` (`false`).
pub fn resolve_settings_path() -> (PathBuf, bool) {
  let exe_dir = env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(Path::to_path_buf));

  if let Some(dir) = &exe_dir {
    let portable_marker = dir.join(PORTABLE_MARKER_FILE);
    let portable_json = dir.join(PORTABLE_SETTINGS_FILE);
    if portable_marker.exists() || portable_json.exists() {
      return (portable_json, true);
    }
  }

  if let Some(appdata) = env::var_os("APPDATA") {
    let appdata_path = PathBuf::from(appdata);
    if !appdata_path.as_os_str().is_empty() {
      return (
        appdata_path
          .join(APPDATA_FOLDER_NAME)
          .join(APPDATA_SETTINGS_FILE),
        false,
      );
    }
  }

  if let Some(user_profile) = env::var_os("USERPROFILE") {
    let profile_path = PathBuf::from(user_profile);
    if !profile_path.as_os_str().is_empty() {
      return (
        profile_path
          .join("AppData")
          .join("Roaming")
          .join(APPDATA_FOLDER_NAME)
          .join(APPDATA_SETTINGS_FILE),
        false,
      );
    }
  }

  let fallback_dir = exe_dir.unwrap_or_else(|| PathBuf::from("."));
  (fallback_dir.join(PORTABLE_SETTINGS_FILE), true)
}

/// Verwaltet das Laden, das gedrosselte Hintergrund-Speichern (~400 ms Debounce) sowie das
/// sofortige atomare Ausschreiben (`.tmp` + `rename`) beim Beenden von FrameForge.
pub struct SettingsManager {
  settings_path: PathBuf,
  portable: bool,
  last_saved: AppSettings,
  pending: Option<AppSettings>,
  last_change_instant: Option<Instant>,
}

impl SettingsManager {
  /// Laedt vorhandene Einstellungen von der Festplatte oder erzeugt saubere Standardwerte.
  pub fn load() -> (AppSettings, Self) {
    let (settings_path, portable) = resolve_settings_path();
    let loaded = fs::read_to_string(&settings_path)
      .ok()
      .and_then(|raw| serde_json::from_str::<AppSettings>(&raw).ok())
      .map(AppSettings::sanitized)
      .unwrap_or_else(|| AppSettings::default().sanitized());

    let mgr = Self {
      settings_path,
      portable,
      last_saved: loaded.clone(),
      pending: None,
      last_change_instant: None,
    };
    (loaded, mgr)
  }

  /// Meldet eine Aenderung zum gedrosselten Autosave an.
  /// Identische Folgemeldungen aus der Render-Schleife setzen den 400-ms-Timer nicht neu zurueck.
  pub fn request_save(&mut self, current: &AppSettings) {
    if current == &self.last_saved {
      self.pending = None;
      self.last_change_instant = None;
      return;
    }
    if self.pending.as_ref() == Some(current) {
      return;
    }
    self.pending = Some(current.clone());
    self.last_change_instant = Some(Instant::now());
  }

  /// Prueft pro Frame kostenguenstig, ob das 400-ms-Debounce-Fenster abgelaufen ist,
  /// und schreibt bei Bedarf atomar auf die Festplatte.
  pub fn tick_autosave(&mut self) -> bool {
    let Some(changed_at) = self.last_change_instant else {
      return false;
    };
    if changed_at.elapsed() < AUTOSAVE_DEBOUNCE {
      return false;
    }
    let Some(snapshot) = self.pending.take() else {
      self.last_change_instant = None;
      return false;
    };
    self.last_change_instant = None;

    if write_settings_atomic(&self.settings_path, &snapshot).is_ok() {
      self.last_saved = snapshot;
      true
    } else {
      false
    }
  }

  /// Schreibt offene oder uebergebene Aenderungen sofort atomar (`.tmp` + `rename`) beim Beenden.
  pub fn flush_now(&mut self, current: &AppSettings) -> bool {
    let target = if current != &self.last_saved {
      Some(current.clone())
    } else {
      self.pending.take()
    };
    self.pending = None;
    self.last_change_instant = None;

    let Some(snapshot) = target else {
      return false;
    };
    if write_settings_atomic(&self.settings_path, &snapshot).is_ok() {
      self.last_saved = snapshot;
      true
    } else {
      false
    }
  }

  #[inline]
  pub fn is_portable(&self) -> bool {
    self.portable
  }

  pub fn path_display(&self) -> String {
    self.settings_path.display().to_string()
  }
}

fn write_settings_atomic(path: &Path, settings: &AppSettings) -> io::Result<()> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)?;
    }
  }

  let mut payload = serde_json::to_string_pretty(settings)
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
  payload.push('\n');

  let tmp_name = match path.file_name().and_then(|n| n.to_str()) {
    Some(name) => format!("{name}.tmp"),
    None => String::from("settings.json.tmp"),
  };
  let tmp_path = path.with_file_name(tmp_name);

  {
    let mut tmp_file = File::create(&tmp_path)?;
    tmp_file.write_all(payload.as_bytes())?;
    tmp_file.sync_all()?;
  }

  match fs::rename(&tmp_path, path) {
    Ok(()) => Ok(()),
    Err(_) => {
      let write_res = fs::write(path, payload.as_bytes());
      let _ = fs::remove_file(&tmp_path);
      write_res
    }
  }
}
