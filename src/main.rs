#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod backend;
mod capture;
mod config;
mod dlss45;
mod dx12;
mod fsr3;
mod gbuffer;
mod i18n;
mod logo;
mod telemetry;
mod ui;
mod ui_renderer;
mod xess3;

use std::ffi::c_void;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::HWND;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::windows::{WindowAttributesExtWindows, WindowExtWindows};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::{list_audio_input_devices, list_audio_output_devices, AudioManager};
use crate::backend::{FrameGenBackend, PassthroughBackend};
use crate::capture::{list_capture_devices, CaptureConfig, CaptureManager};
use crate::config::{AppSettings, SettingsManager};
use crate::dx12::{Dx12Context, PerformanceTurboConfig, VideoRenderParams};
use crate::i18n::{tr, AppLanguage};
use crate::telemetry::{FrameTelemetrySample, OsdConfig, SystemTelemetry};
use crate::ui::{
  configure_gaming_theme, draw_ui, output_fps_limit_label, CaptureHudStats, DebugViewMode,
  FgHudStats, LogStore, UiActions, UiState, UpscalerBackend,
};

const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;
const WINDOW_TITLE: &str = "FrameForge v1.0 - DX12";

struct FrameForgeApp {
  settings_mgr: SettingsManager,
  initial_settings: AppSettings,
  last_exported_settings: AppSettings,
  audio_manager: AudioManager,
  capture_manager: CaptureManager,
  backend: Box<dyn FrameGenBackend>,
  dx12: Option<Dx12Context>,
  egui_state: Option<egui_winit::State>,
  egui_ctx: egui::Context,
  ui: UiState,
  osd_config: OsdConfig,
  logs: LogStore,
  telemetry: SystemTelemetry,
  window: Option<Window>,
  next_frame_deadline: Instant,
  last_logged_backend: UpscalerBackend,
  last_logged_fg_enabled: bool,
  last_logged_fg_multiplier: u32,
  last_logged_fg_status: String,
  last_logged_turbo: PerformanceTurboConfig,
  last_logged_fps_limit: u32,
  last_logged_capture_cfg: Option<CaptureConfig>,
  last_logged_audio_in: Option<String>,
  last_logged_audio_out: Option<String>,
}

impl Default for FrameForgeApp {
  fn default() -> Self {
    let egui_ctx = egui::Context::default();
    configure_gaming_theme(&egui_ctx);

    let (initial_settings, settings_mgr) = SettingsManager::load();
    let mut ui = UiState::default();
    let mut osd_config = OsdConfig::default();

    ui.settings_is_portable = settings_mgr.is_portable();
    ui.settings_path_display = settings_mgr.path_display();
    ui.apply_settings(&initial_settings, &mut osd_config);

    let lang = ui.language;
    let mut logs = LogStore::new();
    let mode_text = if settings_mgr.is_portable() {
      tr(lang, "Portable-Modus", "Portable mode")
    } else {
      tr(lang, "AppData-Modus", "AppData mode")
    };
    logs.info(match lang {
      AppLanguage::De => format!(
        "[FrameForge] v1.0.0 initialisiert ({mode_text}: {}).",
        settings_mgr.path_display()
      ),
      AppLanguage::En => format!(
        "[FrameForge] v1.0.0 initialized ({mode_text}: {}).",
        settings_mgr.path_display()
      ),
    });

    let audio_manager = AudioManager::new(
      ui.audio_enabled,
      ui.audio_muted,
      ui.audio_volume_factor(),
    );

    let last_logged_backend = ui.upscaler_backend;
    let last_logged_fg_enabled = ui.frame_generation_enabled;
    let last_logged_fg_multiplier = ui.fg_multiplier.clamp(2, 4);
    let last_logged_turbo = ui.turbo_config();
    let last_logged_fps_limit = ui.target_output_fps_limit;
    let last_exported_settings = ui.export_settings(&osd_config);

    Self {
      settings_mgr,
      initial_settings,
      last_exported_settings,
      audio_manager,
      capture_manager: CaptureManager::new(),
      backend: Box::new(PassthroughBackend::new()),
      dx12: None,
      egui_state: None,
      egui_ctx,
      ui,
      osd_config,
      logs,
      telemetry: SystemTelemetry::new(),
      window: None,
      next_frame_deadline: Instant::now(),
      last_logged_backend,
      last_logged_fg_enabled,
      last_logged_fg_multiplier,
      last_logged_fg_status: String::from("Inaktiv"),
      last_logged_turbo,
      last_logged_fps_limit,
      last_logged_capture_cfg: None,
      last_logged_audio_in: None,
      last_logged_audio_out: None,
    }
  }
}

impl FrameForgeApp {
  fn extract_win32_hwnd(window: &Window) -> Result<HWND> {
    let handle = window
      .window_handle()
      .context("Failed to query raw window handle")?;
    match handle.as_raw() {
      RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut c_void)),
      _ => bail!("FrameForge requires a native Win32 window (HWND)"),
    }
  }

  fn toggle_borderless_fullscreen(&mut self) {
    let Some(window) = &self.window else {
      return;
    };
    let lang = self.ui.language;
    if window.fullscreen().is_some() {
      window.set_fullscreen(None);
      self.logs.info(tr(
        lang,
        "[Anzeige] Gewechselt in den regulären Fenstermodus.",
        "[Display] Switched to standard windowed mode.",
      ));
    } else {
      window.set_fullscreen(Some(Fullscreen::Borderless(None)));
      self.logs.info(tr(
        lang,
        "[Anzeige] Randloses Vollbild (Borderless Fullscreen) aktiviert.",
        "[Display] Enabled borderless fullscreen mode.",
      ));
    }
  }

  fn refresh_devices_and_sync(&mut self) {
    let devices = list_capture_devices(&mut self.logs);
    self.ui.apply_scanned_devices(devices);
    self.sync_capture_stream();
  }

  fn refresh_audio_devices_and_sync(&mut self) {
    let inputs = list_audio_input_devices(&mut self.logs);
    let outputs = list_audio_output_devices(&mut self.logs);
    self
      .ui
      .apply_scanned_audio_devices(inputs, outputs, &mut self.logs);
    self.sync_audio_engine();
  }

  fn sync_capture_stream(&mut self) {
    let lang = self.ui.language;
    match self.ui.desired_capture_config() {
      Some(cfg) => {
        if self.capture_manager.active_config() != Some(&cfg) {
          if let Some(dx12) = &mut self.dx12 {
            dx12.clear_video_signal();
          }
          if self.last_logged_capture_cfg.as_ref() != Some(&cfg) {
            self.logs.info(match lang {
              AppLanguage::De => format!(
                "[Capture] Videoquelle aktiv: {} ({}, {}).",
                cfg.device_name,
                cfg.target_resolution.label(lang),
                cfg.input_fps.label(lang)
              ),
              AppLanguage::En => format!(
                "[Capture] Video source active: {} ({}, {}).",
                cfg.device_name,
                cfg.target_resolution.label(lang),
                cfg.input_fps.label(lang)
              ),
            });
            self.last_logged_capture_cfg = Some(cfg.clone());
          }
          self
            .capture_manager
            .start_or_reconfigure(cfg, &mut self.logs);
        }
      }
      None => {
        if self.capture_manager.active_config().is_some() {
          self.capture_manager.stop(&mut self.logs);
          self.logs.info(tr(
            lang,
            "[Capture] Videoquelle deaktiviert (Standby).",
            "[Capture] Video source disabled (Standby).",
          ));
        }
        self.last_logged_capture_cfg = None;
        if let Some(dx12) = &mut self.dx12 {
          dx12.clear_video_signal();
        }
      }
    }
  }

  fn sync_audio_engine(&mut self) {
    let lang = self.ui.language;
    let route = self.ui.desired_audio_route();
    let cur_in = route.as_ref().map(|r| r.input_device_name.clone());
    let cur_out = route.as_ref().and_then(|r| r.output_device_name.clone());

    if cur_in != self.last_logged_audio_in || cur_out != self.last_logged_audio_out {
      if let Some(in_name) = &cur_in {
        let out_desc = cur_out.as_deref().unwrap_or_else(|| {
          tr(
            lang,
            "Windows System-Standard",
            "Windows System Default",
          )
        });
        self.logs.info(match lang {
          AppLanguage::De => format!(
            "[Audio] WASAPI-Routing gesetzt: '{}' -> '{}'.",
            in_name, out_desc
          ),
          AppLanguage::En => format!(
            "[Audio] WASAPI routing active: '{}' -> '{}'.",
            in_name, out_desc
          ),
        });
      }
      self.last_logged_audio_in = cur_in;
      self.last_logged_audio_out = cur_out;
    }

    self.audio_manager.sync_settings(
      self.ui.audio_enabled,
      self.ui.audio_muted,
      self.ui.audio_volume_factor(),
      route,
    );
  }

  /// Prueft Aenderungen gegenueber dem letzten Einstellungs-Abbild und meldet sie beim
  /// debounced `SettingsManager` an (bzw. schreibt bei `force_flush` sofort atomar auf die Platte).
  fn sync_settings_persistence(&mut self, osd_dragged: bool, force_flush: bool) {
    let current = self.ui.export_settings(&self.osd_config);
    if force_flush {
      let _ = self.settings_mgr.flush_now(&current);
      self.last_exported_settings = current;
      return;
    }
    if osd_dragged || current != self.last_exported_settings {
      self.settings_mgr.request_save(&current);
      self.last_exported_settings = current;
    }
    let _ = self.settings_mgr.tick_autosave();
  }

  /// Ermittelt das aktive D3D12-Ausgabe-FPS-Limit:
  /// - `None` (`0` = Unbegrenzt / VSync-gekoppelt)
  /// - `Some(fps)` (`1` = Auto passend zu Video/FG bzw. 60 FPS im Standby, oder feste Stufe `60/120/144/240`)
  fn effective_output_fps_limit(&self) -> Option<u32> {
    match self.ui.target_output_fps_limit {
      0 => None,
      1 => {
        if !self.capture_manager.has_live_signal() {
          Some(60)
        } else {
          let mult = if !self.ui.upscaler_backend.is_fg_backend()
            || !self.ui.frame_generation_enabled
          {
            1_u32
          } else if let Some(dx12) = &self.dx12 {
            let dm = dx12.active_fg_multiplier();
            if dm >= 2 {
              dm
            } else {
              self.ui.configured_fg_multiplier()
            }
          } else {
            self.ui.configured_fg_multiplier()
          };

          let latched_eff = self.telemetry.latched_effective_video_fps();
          let effective_video_fps = if latched_eff >= 10.0_f32 {
            latched_eff
          } else {
            let unique_fps = self.capture_manager.measured_fps();
            let base_fps = if unique_fps >= 5.0_f32 {
              unique_fps
            } else {
              self.ui.input_fps.as_u32() as f32
            };
            base_fps * (mult as f32)
          };
          Some((effective_video_fps.round() as u32).clamp(60, 240))
        }
      }
      fixed => Some(fixed.clamp(30, 1000)),
    }
  }

  fn backend_log_tag(backend: UpscalerBackend) -> &'static str {
    match backend {
      UpscalerBackend::Passthrough => "[Pipeline]",
      UpscalerBackend::AmdFsr3 => "[FSR]",
      UpscalerBackend::IntelXeSSFg => "[XeSS3]",
      UpscalerBackend::NvidiaDlssFg => "[DLSS4.5]",
    }
  }

  fn sync_pipeline_and_fg_logs(&mut self) {
    let lang = self.ui.language;
    let on_txt = tr(lang, "Aktiviert", "Enabled");
    let off_txt = tr(lang, "Deaktiviert", "Disabled");

    let cur_turbo = self.ui.turbo_config();
    if cur_turbo != self.last_logged_turbo {
      if cur_turbo.half_res_flow_scale != self.last_logged_turbo.half_res_flow_scale {
        let state = if cur_turbo.half_res_flow_scale {
          on_txt
        } else {
          off_txt
        };
        self.logs.info(format!(
          "{}: {state}.",
          tr(
            lang,
            "[Turbo] G-Buffer Flow-Scale 50 % (halbe Auflösung)",
            "[Turbo] G-Buffer 50% Flow Scale (half resolution)"
          )
        ));
      }
      if cur_turbo.fast_gbuffer_search != self.last_logged_turbo.fast_gbuffer_search {
        let state = if cur_turbo.fast_gbuffer_search {
          on_txt
        } else {
          off_txt
        };
        self.logs.info(format!(
          "{}: {state}.",
          tr(
            lang,
            "[Turbo] Schneller G-Buffer-Suchradius (5-Tap-Diamond)",
            "[Turbo] Fast G-Buffer search radius (5-tap diamond)"
          )
        ));
      }
      if cur_turbo.force_fast_compute_fg != self.last_logged_turbo.force_fast_compute_fg {
        let state = if cur_turbo.force_fast_compute_fg {
          on_txt
        } else {
          off_txt
        };
        self.logs.info(format!(
          "{}: {state}.",
          tr(
            lang,
            "[Turbo] iGPU-Leichtgewicht-Interpolator (Compute-Pfad)",
            "[Turbo] Lightweight iGPU interpolator (compute path)"
          )
        ));
      }
      if cur_turbo.hud_protection != self.last_logged_turbo.hud_protection {
        let state = if cur_turbo.hud_protection {
          on_txt
        } else {
          off_txt
        };
        self.logs.info(format!(
          "{}: {state}.",
          tr(
            lang,
            "[Turbo] Automatischer HUD-, Minimap- & Untertitel-Schutz",
            "[Turbo] Automatic HUD, minimap & subtitle protection"
          )
        ));
      }
      self.last_logged_turbo = cur_turbo;
    }

    if self.ui.target_output_fps_limit != self.last_logged_fps_limit {
      let lbl = output_fps_limit_label(self.ui.target_output_fps_limit, lang);
      self.logs.info(format!(
        "{}: {lbl}.",
        tr(
          lang,
          "[Anzeige] D3D12-Ausgabe-FPS-Limit gesetzt auf",
          "[Display] D3D12 output FPS limit set to"
        )
      ));
      self.last_logged_fps_limit = self.ui.target_output_fps_limit;
    }

    let cur_status = self
      .dx12
      .as_ref()
      .map(|d| d.active_fg_status_label())
      .unwrap_or_else(|| String::from("Inaktiv"));
    let is_native = self
      .dx12
      .as_ref()
      .map(|d| d.active_fg_is_native_sdk())
      .unwrap_or(false);
    let cur_mult = self.ui.fg_multiplier.clamp(2, 4);

    if self.ui.upscaler_backend != self.last_logged_backend
      || self.ui.frame_generation_enabled != self.last_logged_fg_enabled
      || (self.ui.upscaler_backend.supports_multi_fg()
        && cur_mult != self.last_logged_fg_multiplier)
    {
      let tag = Self::backend_log_tag(self.ui.upscaler_backend);
      match self.ui.upscaler_backend {
        UpscalerBackend::Passthrough => {
          self.logs.info(tr(
            lang,
            "[Pipeline] Backend gewechselt auf: Aus (Passthrough).",
            "[Pipeline] Backend switched to: Off (Passthrough).",
          ));
        }
        UpscalerBackend::AmdFsr3 | UpscalerBackend::IntelXeSSFg | UpscalerBackend::NvidiaDlssFg => {
          let eff_mult = if self.ui.upscaler_backend == UpscalerBackend::AmdFsr3 {
            2
          } else {
            cur_mult
          };
          if self.ui.frame_generation_enabled {
            let mode_detail = if cur_status == "Inaktiv" {
              String::from(tr(
                lang,
                "bereit — wartet auf aktives HDMI-Videosignal",
                "ready — waiting for active HDMI video signal",
              ))
            } else if is_native {
              match lang {
                AppLanguage::De => format!("Natives Hersteller-SDK aktiv ({cur_status})"),
                AppLanguage::En => format!("Native vendor SDK active ({cur_status})"),
              }
            } else {
              format!("DX12 Multi-FG Compute-Engine ({cur_status})")
            };
            self.logs.info(match lang {
              AppLanguage::De => format!(
                "{tag} Aktiviert: {} ({}x Frame Generation) | {mode_detail}.",
                self.ui.upscaler_backend.label(lang),
                eff_mult
              ),
              AppLanguage::En => format!(
                "{tag} Enabled: {} ({}x Frame Generation) | {mode_detail}.",
                self.ui.upscaler_backend.label(lang),
                eff_mult
              ),
            });
          } else {
            self.logs.info(match lang {
              AppLanguage::De => format!(
                "{tag} {} gewählt (Zwischenbild-Generierung aktuell pausiert).",
                self.ui.upscaler_backend.label(lang)
              ),
              AppLanguage::En => format!(
                "{tag} {} selected (Frame Generation currently paused).",
                self.ui.upscaler_backend.label(lang)
              ),
            });
          }
        }
      }
      self.last_logged_backend = self.ui.upscaler_backend;
      self.last_logged_fg_enabled = self.ui.frame_generation_enabled;
      self.last_logged_fg_multiplier = cur_mult;
    }

    if cur_status != self.last_logged_fg_status {
      if cur_status != "Inaktiv" && cur_status != "Passthrough" {
        let tag = Self::backend_log_tag(self.ui.upscaler_backend);
        if is_native {
          self.logs.info(match lang {
            AppLanguage::De => format!("{tag} Laufzeit-Status: Natives SDK aktiv ({cur_status})."),
            AppLanguage::En => format!("{tag} Runtime status: Native SDK active ({cur_status})."),
          });
        } else {
          self.logs.info(match lang {
            AppLanguage::De => {
              format!("{tag} Laufzeit-Status: DX12 Compute-Engine bereit ({cur_status}).")
            }
            AppLanguage::En => {
              format!("{tag} Runtime status: DX12 compute engine ready ({cur_status}).")
            }
          });
        }
      }
      self.last_logged_fg_status = cur_status;
    }
  }
}

impl ApplicationHandler for FrameForgeApp {
  fn resumed(&mut self, event_loop: &ActiveEventLoop) {
    if self.window.is_some() {
      return;
    }
    let lang = self.ui.language;
    let attrs = Window::default_attributes()
      .with_title(WINDOW_TITLE)
      .with_inner_size(PhysicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
      .with_visible(false)
      .with_window_icon(crate::logo::create_window_icon())
      .with_taskbar_icon(crate::logo::create_taskbar_icon());

    let window = match event_loop.create_window(attrs) {
      Ok(w) => w,
      Err(e) => {
        self.logs.error(match lang {
          AppLanguage::De => format!("[FrameForge] Fenstererstellung fehlgeschlagen: {e}"),
          AppLanguage::En => format!("[FrameForge] Window creation failed: {e}"),
        });
        event_loop.exit();
        return;
      }
    };
    window.set_taskbar_icon(crate::logo::create_taskbar_icon());
    window.set_window_icon(crate::logo::create_window_icon());
    if let Ok(wh) = window.window_handle() {
      if let RawWindowHandle::Win32(h) = wh.as_raw() {
        crate::logo::apply_win32_window_icons(h.hwnd.get() as isize);
      }
    }
    window.set_visible(true);
    if let Ok(wh) = window.window_handle() {
      if let RawWindowHandle::Win32(h) = wh.as_raw() {
        crate::logo::apply_win32_window_icons(h.hwnd.get() as isize);
      }
    }

    let sz = window.inner_size();
    let (width, height) = (sz.width.max(1), sz.height.max(1));
    let hwnd = match Self::extract_win32_hwnd(&window) {
      Ok(h) => h,
      Err(e) => {
        self.logs.error(match lang {
          AppLanguage::De => format!("[FrameForge] HWND-Extraktion fehlgeschlagen: {e:#}"),
          AppLanguage::En => format!("[FrameForge] HWND extraction failed: {e:#}"),
        });
        event_loop.exit();
        return;
      }
    };
    let dx12 = match Dx12Context::new(hwnd, width, height, &mut self.logs) {
      Ok(c) => c,
      Err(e) => {
        self.logs.error(match lang {
          AppLanguage::De => {
            format!("[FrameForge] DirectX-12-Initialisierung fehlgeschlagen: {e:#}")
          }
          AppLanguage::En => format!("[FrameForge] DirectX 12 initialization failed: {e:#}"),
        });
        event_loop.exit();
        return;
      }
    };
    if let Err(e) = self.backend.initialize(
      dx12.device(),
      dx12.swapchain(),
      width,
      height,
      &mut self.logs,
    ) {
      self.logs.error(match lang {
        AppLanguage::De => format!(
          "[FrameForge] Backend-Initialisierung ({}) fehlgeschlagen: {e:#}",
          self.backend.name()
        ),
        AppLanguage::En => format!(
          "[FrameForge] Backend initialization ({}) failed: {e:#}",
          self.backend.name()
        ),
      });
      event_loop.exit();
      return;
    }

    let tearing_ok = dx12.tearing_supported();
    self.last_logged_fg_status = dx12.active_fg_status_label();
    let egui_state = egui_winit::State::new(
      self.egui_ctx.clone(),
      egui::ViewportId::ROOT,
      &window,
      Some(window.scale_factor() as f32),
      None,
      None,
    );

    self.egui_state = Some(egui_state);
    self.dx12 = Some(dx12);
    self.window = Some(window);

    // Angeschlossene Video- & Audio-Geraete scannen und initial_settings inkl. gespeicherter
    // Geraetenamen automatisch anwenden & starten:
    let video_devices = list_capture_devices(&mut self.logs);
    self.ui.apply_scanned_devices(video_devices);

    let audio_inputs = list_audio_input_devices(&mut self.logs);
    let audio_outputs = list_audio_output_devices(&mut self.logs);
    self
      .ui
      .apply_scanned_audio_devices(audio_inputs, audio_outputs, &mut self.logs);

    self
      .ui
      .apply_settings(&self.initial_settings, &mut self.osd_config);
    if !tearing_ok {
      self.ui.allow_tearing = false;
    }

    self.sync_capture_stream();
    self.sync_audio_engine();
    self.last_exported_settings = self.ui.export_settings(&self.osd_config);

    let ready_msg = match lang {
      AppLanguage::De => format!(
        "[FrameForge] Bereit | {} ({}x{}) | ESC=Menü | F2=OSD | F3=Profil | F4=G-Buffer-Debug | M=Mute | F11=Vollbild",
        WINDOW_TITLE, width, height
      ),
      AppLanguage::En => format!(
        "[FrameForge] Ready | {} ({}x{}) | ESC=Menu | F2=OSD | F3=Preset | F4=G-Buffer Debug | M=Mute | F11=Fullscreen",
        WINDOW_TITLE, width, height
      ),
    };
    self.logs.info(ready_msg);

    self.next_frame_deadline = Instant::now();
    if let Some(w) = &self.window {
      w.request_redraw();
    }
  }

  fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
    if !self
      .window
      .as_ref()
      .map(|w| w.id() == window_id)
      .unwrap_or(false)
    {
      return;
    }

    let mut hotkey_changed_settings = false;
    if let WindowEvent::KeyboardInput {
      event:
        KeyEvent {
          physical_key: PhysicalKey::Code(code),
          state: ElementState::Pressed,
          repeat: false,
          ..
        },
      ..
    } = &event
    {
      let lang = self.ui.language;
      match code {
        KeyCode::Escape => self.ui.menu_visible = !self.ui.menu_visible,
        KeyCode::F11 => self.toggle_borderless_fullscreen(),
        KeyCode::F2 => {
          self.osd_config.enabled = !self.osd_config.enabled;
          hotkey_changed_settings = true;
          let state_lbl = if self.osd_config.enabled {
            tr(lang, "Ein", "On")
          } else {
            tr(lang, "Aus", "Off")
          };
          self.logs.info(format!(
            "{}: {state_lbl}",
            tr(
              lang,
              "[OSD] Performance-Overlay per F2 umgeschaltet",
              "[OSD] Performance overlay toggled via F2"
            )
          ));
        }
        KeyCode::F3 => {
          let next = self.osd_config.cycle_preset();
          hotkey_changed_settings = true;
          self.logs.info(format!(
            "{}: {}",
            tr(
              lang,
              "[OSD] Telemetrie-Profil per F3 gewechselt auf",
              "[OSD] Telemetry preset switched via F3 to"
            ),
            next.label(lang)
          ));
        }
        KeyCode::F4 => {
          self.ui.debug_view_mode = self.ui.debug_view_mode.cycle_next();
          if self.ui.debug_view_mode != DebugViewMode::Normal && !self.ui.gbuffer_enabled {
            self.ui.gbuffer_enabled = true;
            hotkey_changed_settings = true;
            self.logs.info(tr(
              lang,
              "[GBuffer] Synthetic G-Buffer für Live-Debug-Ansicht automatisch aktiviert.",
              "[GBuffer] Automatically enabled Synthetic G-Buffer for live debug view.",
            ));
          }
          self.logs.info(format!(
            "{}: {}",
            tr(
              lang,
              "[GBuffer] Ansichts-Modus per F4 gewechselt auf",
              "[GBuffer] Debug view switched via F4 to"
            ),
            self.ui.debug_view_mode.label(lang)
          ));
        }
        KeyCode::KeyM if !self.egui_ctx.wants_keyboard_input() => {
          self.ui.audio_muted = !self.ui.audio_muted;
          hotkey_changed_settings = true;
          self.sync_audio_engine();
          let mute_lbl = if self.ui.audio_muted {
            tr(lang, "Aktiv", "Muted")
          } else {
            tr(lang, "Aufgehoben", "Unmuted")
          };
          self.logs.info(format!(
            "{}: {mute_lbl}",
            tr(
              lang,
              "[Audio] Stummschaltung per Taste M",
              "[Audio] Audio mute toggled via Key M"
            )
          ));
        }
        _ => {}
      }
    }

    if hotkey_changed_settings {
      self.sync_settings_persistence(false, false);
    }

    let uncapped = self.effective_output_fps_limit().is_none();
    if let (Some(w), Some(st)) = (self.window.as_ref(), self.egui_state.as_mut()) {
      let resp = st.on_window_event(w, &event);
      if uncapped && resp.repaint {
        w.request_redraw();
      }
    }

    match event {
      WindowEvent::CloseRequested => {
        self.sync_settings_persistence(false, true);
        self.audio_manager.shutdown(&mut self.logs);
        self.capture_manager.stop(&mut self.logs);
        if let Some(dx12) = &mut self.dx12 {
          let _ = dx12.wait_for_gpu();
        }
        event_loop.exit();
      }
      WindowEvent::Resized(ns) if ns.width > 0 && ns.height > 0 => {
        let lang = self.ui.language;
        if let Some(dx12) = &mut self.dx12 {
          let prev = dx12.dimensions();
          match dx12.resize(ns.width, ns.height) {
            Ok(()) if prev != (ns.width, ns.height) => self.logs.info(match lang {
              AppLanguage::De => format!(
                "[FrameForge][DX12] SwapChain skaliert auf {}x{} px.",
                ns.width, ns.height
              ),
              AppLanguage::En => format!(
                "[FrameForge][DX12] SwapChain resized to {}x{} px.",
                ns.width, ns.height
              ),
            }),
            Ok(()) => {}
            Err(e) => self.logs.error(match lang {
              AppLanguage::De => format!("[FrameForge] SwapChain-Resize fehlgeschlagen: {e:#}"),
              AppLanguage::En => format!("[FrameForge] SwapChain resize failed: {e:#}"),
            }),
          }
        }
        if let Err(e) = self.backend.resize(ns.width, ns.height) {
          self.logs.error(match lang {
            AppLanguage::De => format!("[FrameForge] Backend-Resize fehlgeschlagen: {e:#}"),
            AppLanguage::En => format!("[FrameForge] Backend resize failed: {e:#}"),
          });
        }
        if let Some(w) = &self.window {
          w.request_redraw();
        }
      }
      WindowEvent::RedrawRequested => {
        let Some(w) = self.window.as_ref() else {
          return;
        };
        let sz = w.inner_size();
        if sz.width == 0 || sz.height == 0 {
          return;
        }
        let is_fullscreen = w.fullscreen().is_some();

        self.audio_manager.poll_events(&mut self.logs);

        if let Some(cap) = self
          .capture_manager
          .poll_latest_frame(self.ui.duplicate_filter_60_to_30, &mut self.logs)
        {
          if let Some(dx12) = self.dx12.as_mut() {
            if let Err(e) = dx12.upload_capture_frame(
              cap.width,
              cap.height,
              &cap.rgba_data,
              cap.is_duplicate,
              &mut self.logs,
            ) {
              let lang = self.ui.language;
              self.logs.error(match lang {
                AppLanguage::De => {
                  format!("[FrameForge][DX12] Textur-Upload fehlgeschlagen: {e:#}")
                }
                AppLanguage::En => format!("[FrameForge][DX12] Texture upload failed: {e:#}"),
              });
            }
          }
        } else if !self.capture_manager.has_live_signal() {
          if let Some(dx12) = self.dx12.as_mut() {
            dx12.clear_video_signal();
          }
        }

        let mut ui_actions = UiActions::default();
        {
          let (Some(w_ref), Some(st), Some(dx12)) = (
            self.window.as_ref(),
            self.egui_state.as_mut(),
            self.dx12.as_mut(),
          ) else {
            return;
          };
          let raw_input = st.take_egui_input(w_ref);
          let adapter_name = dx12.adapter_name().to_string();
          let tearing_supported = dx12.tearing_supported();
          let swapchain_size = dx12.dimensions();
          let gbuffer_stats = dx12.gbuffer_stats();
          let gbuffer_dims = dx12.gbuffer_dimensions();
          let raw_gbuffer_gpu_ms = {
            let m = dx12.gbuffer_gpu_time_ms();
            if m > 0.0_f32 {
              m
            } else {
              gbuffer_stats.gpu_compute_ms
            }
          };
          let raw_fg_gpu_ms = dx12.fg_gpu_time_ms();
          let fmt_lbl = self.capture_manager.active_format_label().to_string();
          let has_signal = self.capture_manager.has_live_signal();
          let raw_in_fps = self.capture_manager.raw_measured_fps();
          let unique_in_fps = self.capture_manager.measured_fps();
          let raw_dup_pct = self.capture_manager.duplicate_ratio_percent();

          let dx_mult = dx12.active_fg_multiplier();
          let configured_mult = self.ui.configured_fg_multiplier();
          let active_multiplier = if !self.ui.upscaler_backend.is_fg_backend()
            || !self.ui.frame_generation_enabled
          {
            1
          } else if has_signal && dx_mult >= 2 {
            dx_mult
          } else {
            configured_mult
          };
          let raw_effective_video_fps = if !has_signal {
            0.0_f32
          } else {
            unique_in_fps * (active_multiplier as f32)
          };
          let aud_tel = self.audio_manager.telemetry();

          // Interne Frame-Akkumulation + beruhigter LatchedSnapshot im gewaehlten Intervall
          self.telemetry.record_frame_and_poll(
            FrameTelemetrySample {
              has_signal,
              capture_raw_fps: raw_in_fps,
              capture_unique_fps: unique_in_fps,
              capture_dup_pct: raw_dup_pct,
              gbuffer_gpu_ms: raw_gbuffer_gpu_ms,
              fg_gpu_ms: raw_fg_gpu_ms,
              active_fg_multiplier: active_multiplier,
              effective_video_fps: raw_effective_video_fps,
              audio_latency_ms: aud_tel.buffer_latency_ms,
            },
            self.osd_config.refresh_interval_ms,
            || dx12.query_vram_info(),
          );

          // Ausschliesslich die beruhigten Snapshot-Zahlen an UI & OSD weiterreichen
          let cap_stats = CaptureHudStats {
            has_signal,
            raw_fps: self.telemetry.latched_capture_raw_fps(),
            unique_fps: self.telemetry.latched_capture_unique_fps(),
            duplicate_ratio_pct: self.telemetry.latched_capture_dup_pct(),
            resolution: self.capture_manager.active_resolution(),
            format_label: &fmt_lbl,
            dropped_duplicates: self.capture_manager.dropped_duplicates(),
          };
          let latched_gbuffer_gpu_ms = self.telemetry.latched_gbuffer_gpu_ms();
          let fg_stats = FgHudStats {
            fg_gpu_ms: self.telemetry.latched_fg_gpu_ms(),
            status_label: dx12.active_fg_status_label(),
            native_sdk: dx12.active_fg_is_native_sdk(),
            active_multiplier,
            effective_video_fps: self.telemetry.latched_effective_video_fps(),
            is_interpolated_phase: dx12.is_presenting_interpolated_frame(),
          };

          let full_output = self.egui_ctx.run(raw_input, |ctx| {
            ui_actions = draw_ui(
              ctx,
              &mut self.ui,
              &mut self.osd_config,
              &mut self.logs,
              &self.telemetry,
              cap_stats,
              gbuffer_stats,
              gbuffer_dims,
              latched_gbuffer_gpu_ms,
              &fg_stats,
              aud_tel,
              &adapter_name,
              tearing_supported,
              swapchain_size,
              is_fullscreen,
            );
          });
          st.handle_platform_output(w_ref, full_output.platform_output);
          let prims = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);

          let vparams = VideoRenderParams {
            delta_time_ms: self.telemetry.live_delta_time_ms(),
            expand_limited_rgb: self.ui.expand_limited_rgb,
            rcas_sharpness: self.ui.cas_sharpness,
            allow_tearing: tearing_supported && self.ui.allow_tearing,
            debug_view_mode: self.ui.debug_view_mode,
            gbuffer: self.ui.gbuffer_params(),
            upscaler_backend: self.ui.upscaler_backend,
            frame_generation_enabled: self.ui.frame_generation_enabled,
            fg_multiplier: self.ui.fg_multiplier.clamp(2, 4),
            turbo: self.ui.turbo_config(),
          };
          if let Err(e) = dx12.render_frame(
            self.backend.as_mut(),
            vparams,
            &prims,
            &full_output.textures_delta,
            full_output.pixels_per_point,
          ) {
            let lang = self.ui.language;
            self.logs.error(match lang {
              AppLanguage::De => format!("[FrameForge] Render-Fehler: {e:#}"),
              AppLanguage::En => format!("[FrameForge] Render error: {e:#}"),
            });
          }
          self.telemetry.observe_fg_phase(
            has_signal && dx12.active_fg_multiplier() >= 2,
            dx12.is_presenting_interpolated_frame(),
          );
        }

        if ui_actions.reset_requested {
          let keep_lang = self.ui.language;
          let defaults = AppSettings {
            language: keep_lang,
            ..AppSettings::default()
          };
          self.ui.apply_settings(&defaults, &mut self.osd_config);
          if let Some(dx12) = &self.dx12 {
            if !dx12.tearing_supported() {
              self.ui.allow_tearing = false;
            }
          }
          self.sync_capture_stream();
          self.sync_audio_engine();
          self.sync_pipeline_and_fg_logs();
          self.sync_settings_persistence(false, true);
        } else {
          self.sync_pipeline_and_fg_logs();
          if ui_actions.refresh_capture_devices {
            self.refresh_devices_and_sync();
          } else {
            self.sync_capture_stream();
          }
          if ui_actions.refresh_audio_devices {
            self.refresh_audio_devices_and_sync();
          } else {
            self.sync_audio_engine();
          }
          self.sync_settings_persistence(ui_actions.osd_dragged, false);
        }

        if ui_actions.toggle_fullscreen {
          self.toggle_borderless_fullscreen();
        }
      }
      _ => {}
    }
  }

  fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let _ = self.settings_mgr.tick_autosave();

    let Some(w) = &self.window else {
      return;
    };

    match self.effective_output_fps_limit() {
      None => {
        self.next_frame_deadline = Instant::now();
        event_loop.set_control_flow(ControlFlow::Poll);
        w.request_redraw();
      }
      Some(limit_fps) => {
        let frame_interval = Duration::from_secs_f64(1.0_f64 / (limit_fps.max(1) as f64));
        let now = Instant::now();

        if self.next_frame_deadline > now + frame_interval {
          self.next_frame_deadline = now + frame_interval;
        }

        if now >= self.next_frame_deadline {
          let advanced = self.next_frame_deadline + frame_interval;
          self.next_frame_deadline = if advanced > now {
            advanced
          } else {
            now + frame_interval
          };
          w.request_redraw();
          event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame_deadline));
        } else {
          event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame_deadline));
        }
      }
    }
  }

  fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
    self.sync_settings_persistence(false, true);
  }
}

fn main() -> Result<()> {
  let event_loop = EventLoop::new().context("Failed to create winit EventLoop")?;
  event_loop.set_control_flow(ControlFlow::Poll);
  let mut app = FrameForgeApp::default();
  event_loop
    .run_app(&mut app)
    .context("Error while running winit EventLoop")?;
  Ok(())
}
