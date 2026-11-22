use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::{nokhwa_initialize, query, Camera};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::i18n::{detect_system_language, tr};
use crate::ui::{InputFps, LogStore, TargetResolution};

static NOKHWA_INIT_ONCE: Once = Once::new();

/// Anzahl verteilter RGB-Stichprobenpixel für die schnelle 60->30-Duplikat-Erkennung.
const DUPLICATE_SAMPLE_COUNT: usize = 512;

/// Stellt sicher, dass `nokhwa_initialize` pro Prozesslauf genau einmal aufgerufen wird.
fn ensure_nokhwa_initialized() {
    NOKHWA_INIT_ONCE.call_once(|| {
        nokhwa_initialize(|_granted| {});
    });
}

/// RAII-Wächter für die Win32-COM-Initialisierung im jeweiligen Thread (wichtig für Media Foundation).
///
/// Wurde COM auf dem Thread bereits initialisiert (z. B. durch `winit` im STA-Modus mit
/// `RPC_E_CHANGED_MODE`), schlägt `hr.is_ok()` fehl und `CoUninitialize` wird beim Drop übersprungen.
struct ComApartmentGuard {
    should_uninitialize: bool,
}

impl ComApartmentGuard {
    fn new_multithreaded() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            should_uninitialize: hr.is_ok(),
        }
    }
}

impl Drop for ComApartmentGuard {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Beschreibt eine über Microsoft Media Foundation gefundene Video-/HDMI-Capture-Quelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDeviceInfo {
    pub index: CameraIndex,
    pub name: String,
    pub description: String,
}

impl CaptureDeviceInfo {
    /// Kompakte, rein ASCII-/Umlaut-sichere Beschriftung für das UI-Dropdown.
    pub fn display_label(&self) -> String {
        format!("[{}] {}", self.index, self.name)
    }

    /// Erkennt typische USB-/PCIe-HDMI-Capture-Cards anhand gängiger Hardware-Bezeichnungen,
    /// damit beim App-Start bevorzugt die Capture-Card statt einer integrierten Laptop-Webcam gewählt wird.
    pub fn looks_like_hdmi_capture(&self) -> bool {
        let lower = format!("{} {}", self.name, self.description).to_lowercase();
        const KEYWORDS: [&str; 13] = [
            "capture",
            "hdmi",
            "elgato",
            "avermedia",
            "cam link",
            "usb video",
            "usb3",
            "mirabox",
            "genki",
            "ugreen",
            "evga",
            "magewell",
            "blackmagic",
        ];
        KEYWORDS.iter().any(|kw| lower.contains(kw))
    }
}

/// Dekodiertes RGBA8-Einzelbild (4 Bytes pro Pixel) aus dem dedizierten Capture-Thread.
#[allow(dead_code)]
pub struct CapturedFrame {
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub frame_index: u64,
    pub timestamp: Instant,
    pub is_duplicate: bool,
    pub source_format: String,
}

/// Wunsch-Konfiguration für den `CaptureManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub device_index: CameraIndex,
    pub device_name: String,
    pub target_resolution: TargetResolution,
    pub input_fps: InputFps,
}

/// Status- und Diagnosemeldungen aus dem Hintergrund-Capture-Thread für den `LogStore` im Hauptthread.
enum CaptureWorkerEvent {
    Info(String),
    Warn(String),
    Error(String),
    StreamOpened {
        width: u32,
        height: u32,
        fps: u32,
        format_label: String,
    },
    StreamStopped,
}

/// Listet alle aktuell über Microsoft Media Foundation (`ApiBackend::MediaFoundation`) verfügbaren
/// Video-Quellen absturzsicher auf. Wirft niemals einen Panic, falls keine Karte angeschlossen ist.
pub fn list_capture_devices(logs: &mut LogStore) -> Vec<CaptureDeviceInfo> {
    let _com = ComApartmentGuard::new_multithreaded();
    ensure_nokhwa_initialized();
    let lang = detect_system_language();

    match query(ApiBackend::MediaFoundation) {
        Ok(mut raw_devices) => {
            raw_devices.sort_by_key(|info| info.index().to_string());

            let devices: Vec<CaptureDeviceInfo> = raw_devices
                .into_iter()
                .map(|info| {
                    let clean_name = sanitize_ascii_label(&info.human_name());
                    let clean_desc = sanitize_ascii_label(info.description());
                    CaptureDeviceInfo {
                        index: info.index().clone(),
                        name: if clean_name.is_empty() {
                            String::from(tr(
                                lang,
                                "Unbekannte Video-Quelle",
                                "Unknown Video Source",
                            ))
                        } else {
                            clean_name
                        },
                        description: clean_desc,
                    }
                })
                .collect();

            if devices.is_empty() {
                logs.info(tr(
                    lang,
                    "[Capture] Media-Foundation-Scan abgeschlossen: Keine Capture-Card gefunden.",
                    "[Capture] Media Foundation scan complete: No capture card found.",
                ));
            } else {
                let names: Vec<String> = devices.iter().map(|d| d.display_label()).collect();
                logs.info(format!(
                    "[Capture] {} {}: {}",
                    devices.len(),
                    tr(
                        lang,
                        "Video-Quelle(n) erkannt",
                        "video source(s) detected"
                    ),
                    names.join(", ")
                ));
            }

            devices
        }
        Err(err) => {
            logs.warn(format!(
                "{} ({err}). {}",
                tr(
                    lang,
                    "[Capture] Media-Foundation-Geräteabfrage ohne Treffer",
                    "[Capture] Media Foundation device query returned no devices"
                ),
                tr(lang, "Fallback auf Standby.", "Falling back to standby.")
            ));
            Vec::new()
        }
    }
}

/// Ersetzt exotische Sonderzeichen in Treiber-Namen durch saubere ASCII-/Umlaut-Zeichen,
/// damit die `egui`-Standardschrift niemals Platzhalter-Kästchen zeichnet.
fn sanitize_ascii_label(raw: &str) -> String {
    raw.chars()
        .filter(|&c| {
            c.is_ascii_graphic()
                || c == ' '
                || matches!(c, 'ä' | 'ö' | 'ü' | 'Ä' | 'Ö' | 'Ü' | 'ß')
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Verwaltet den dedizierten Niedriglatenz-Capture-Thread inkl. Zero-Backlog-Channel (`bounded(1)`),
/// Duplikat-Filterung und geglätteter Eingangs-FPS-Messung.
pub struct CaptureManager {
    running: Option<Arc<AtomicBool>>,
    worker_handle: Option<JoinHandle<()>>,
    frame_rx: Option<Receiver<CapturedFrame>>,
    event_rx: Option<Receiver<CaptureWorkerEvent>>,
    active_config: Option<CaptureConfig>,
    has_signal: bool,
    active_resolution: Option<(u32, u32)>,
    active_format_label: String,
    last_raw_frame_instant: Option<Instant>,
    smoothed_raw_interval_ms: f32,
    raw_fps_initialized: bool,
    last_unique_frame_instant: Option<Instant>,
    smoothed_frame_interval_ms: f32,
    fps_initialized: bool,
    smoothed_duplicate_pct: f32,
    received_frames: u64,
    dropped_duplicates: u64,
}

impl Default for CaptureManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureManager {
    pub fn new() -> Self {
        Self {
            running: None,
            worker_handle: None,
            frame_rx: None,
            event_rx: None,
            active_config: None,
            has_signal: false,
            active_resolution: None,
            active_format_label: String::new(),
            last_raw_frame_instant: None,
            smoothed_raw_interval_ms: 0.0_f32,
            raw_fps_initialized: false,
            last_unique_frame_instant: None,
            smoothed_frame_interval_ms: 0.0_f32,
            fps_initialized: false,
            smoothed_duplicate_pct: 0.0_f32,
            received_frames: 0,
            dropped_duplicates: 0,
        }
    }

    /// Startet oder wechselt den Capture-Stream sauber auf `config`.
    /// Läuft bereits ein identisch konfigurierter Stream, bleibt er unterbrechungsfrei aktiv.
    pub fn start_or_reconfigure(&mut self, config: CaptureConfig, logs: &mut LogStore) {
        if self.active_config.as_ref() == Some(&config) && self.worker_handle.is_some() {
            return;
        }

        self.stop(logs);

        let lang = detect_system_language();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);

        // Zero-Backlog-Channel der Kapazität 1: Der Worker verwirft bei vollem Kanal sofort
        // das veraltete Frame über `frame_drop_rx.try_recv()`, sodass niemals Latenz aufläuft.
        let (frame_tx, frame_rx) = bounded::<CapturedFrame>(1);
        let frame_drop_rx = frame_rx.clone();

        let (event_tx, event_rx) = unbounded::<CaptureWorkerEvent>();
        let worker_config = config.clone();

        logs.info(format!(
            "{} '{}' | {}: {} @ {} ...",
            tr(
                lang,
                "[Capture] Starte Stream auf",
                "[Capture] Starting stream on"
            ),
            config.device_name,
            tr(lang, "Ziel", "Target"),
            config.target_resolution.label(lang),
            config.input_fps.label(lang)
        ));

        let spawn_result = thread::Builder::new()
            .name("frameforge-capture".to_string())
            .spawn(move || {
                run_capture_worker(
                    worker_config,
                    worker_running,
                    frame_tx,
                    frame_drop_rx,
                    event_tx,
                );
            });

        match spawn_result {
            Ok(handle) => {
                self.running = Some(running);
                self.worker_handle = Some(handle);
                self.frame_rx = Some(frame_rx);
                self.event_rx = Some(event_rx);
                self.active_config = Some(config);
            }
            Err(err) => {
                logs.error(format!(
                    "{}: {err}",
                    tr(
                        lang,
                        "[Capture] Capture-Thread konnte nicht gestartet werden",
                        "[Capture] Failed to spawn capture worker thread"
                    )
                ));
            }
        }
    }

    /// Stoppt den laufenden Capture-Thread geordnet und gibt die Capture-Card frei.
    pub fn stop(&mut self, logs: &mut LogStore) {
        let had_active_worker = self.worker_handle.is_some();

        if let Some(flag) = self.running.take() {
            flag.store(false, Ordering::SeqCst);
        }

        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Noch ausstehende Worker-Meldungen vor dem Aufräumen ins Log übernehmen.
        self.drain_worker_events(logs);

        self.frame_rx = None;
        self.event_rx = None;
        self.active_config = None;
        self.has_signal = false;
        self.active_resolution = None;
        self.active_format_label.clear();
        self.last_raw_frame_instant = None;
        self.smoothed_raw_interval_ms = 0.0_f32;
        self.raw_fps_initialized = false;
        self.last_unique_frame_instant = None;
        self.smoothed_frame_interval_ms = 0.0_f32;
        self.fps_initialized = false;
        self.smoothed_duplicate_pct = 0.0_f32;

        if had_active_worker {
            let lang = detect_system_language();
            logs.info(tr(
                lang,
                "[Capture] Capture-Stream sauber gestoppt.",
                "[Capture] Capture stream stopped cleanly.",
            ));
        }
    }

    /// Holt Status-Logs sowie (falls vorhanden) das neueste Capture-Frame ab.
    /// Ist `filter_duplicates_60_to_30` aktiv, werden erkannte 60-Hz-Duplikatframes direkt verworfen.
    pub fn poll_latest_frame(
        &mut self,
        filter_duplicates_60_to_30: bool,
        logs: &mut LogStore,
    ) -> Option<CapturedFrame> {
        self.drain_worker_events(logs);

        // Falls der Hintergrund-Thread nach einem Geräte-Abbruch beendet wurde, Handle aufräumen.
        let worker_finished = self
            .worker_handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if worker_finished {
            if let Some(finished) = self.worker_handle.take() {
                let _ = finished.join();
            }
            self.drain_worker_events(logs);
            self.has_signal = false;
        }

        let latest = {
            let rx = self.frame_rx.as_ref()?;
            let mut latest: Option<CapturedFrame> = None;
            while let Ok(frame) = rx.try_recv() {
                latest = Some(frame);
            }
            latest
        };

        let frame = latest?;
        self.received_frames = self.received_frames.wrapping_add(1);
        self.has_signal = true;
        self.active_resolution = Some((frame.width, frame.height));
        self.active_format_label = frame.source_format.clone();

        // 1. Reale Kamera-Eingangs-FPS (vor dem 60->30-Filter) + gleitenden Duplikat-Anteil messen.
        if let Some(prev_raw_ts) = self.last_raw_frame_instant {
            let raw_dt_ms = (frame
                .timestamp
                .saturating_duration_since(prev_raw_ts)
                .as_secs_f64()
                * 1000.0_f64) as f32;
            let clamped_raw_ms = raw_dt_ms.clamp(1.0_f32, 500.0_f32);
            if !self.raw_fps_initialized {
                self.smoothed_raw_interval_ms = clamped_raw_ms;
                self.raw_fps_initialized = true;
            } else {
                const RAW_ALPHA: f32 = 0.12_f32;
                self.smoothed_raw_interval_ms = self.smoothed_raw_interval_ms
                    * (1.0_f32 - RAW_ALPHA)
                    + clamped_raw_ms * RAW_ALPHA;
            }
        }
        self.last_raw_frame_instant = Some(frame.timestamp);

        let dup_sample_pct = if frame.is_duplicate {
            100.0_f32
        } else {
            0.0_f32
        };
        self.smoothed_duplicate_pct =
            self.smoothed_duplicate_pct * 0.92_f32 + dup_sample_pct * 0.08_f32;

        if filter_duplicates_60_to_30 && frame.is_duplicate {
            self.dropped_duplicates = self.dropped_duplicates.wrapping_add(1);
            return None;
        }

        // 2. Effektive Unique-FPS (nach dem 60->30-Duplikatfilter) über akzeptierte Frames glätten.
        if let Some(prev_ts) = self.last_unique_frame_instant {
            let dt_ms = (frame
                .timestamp
                .saturating_duration_since(prev_ts)
                .as_secs_f64()
                * 1000.0_f64) as f32;
            let clamped_ms = dt_ms.clamp(1.0_f32, 500.0_f32);
            if !self.fps_initialized {
                self.smoothed_frame_interval_ms = clamped_ms;
                self.fps_initialized = true;
            } else {
                const ALPHA: f32 = 0.12_f32;
                self.smoothed_frame_interval_ms =
                    self.smoothed_frame_interval_ms * (1.0_f32 - ALPHA) + clamped_ms * ALPHA;
            }
        }
        self.last_unique_frame_instant = Some(frame.timestamp);

        Some(frame)
    }

    /// Gibt zurück, ob aktuell ein Live-Videosignal anliegt (mit Timeout-Prüfung bei abgezogenem Kabel).
    pub fn has_live_signal(&self) -> bool {
        if !self.has_signal {
            return false;
        }
        if let Some(last_ts) = self.last_raw_frame_instant {
            last_ts.elapsed() < Duration::from_millis(2000)
        } else {
            false
        }
    }

    /// Gemessene Roh-Eingangs-FPS der Capture-Card (vor dem `60->30`-Duplikatfilter).
    pub fn raw_measured_fps(&self) -> f32 {
        let Some(last_ts) = self.last_raw_frame_instant else {
            return 0.0_f32;
        };
        if last_ts.elapsed() > Duration::from_millis(1500) {
            return 0.0_f32;
        }
        if self.raw_fps_initialized && self.smoothed_raw_interval_ms > 0.1_f32 {
            1000.0_f32 / self.smoothed_raw_interval_ms
        } else {
            0.0_f32
        }
    }

    /// Effektive Unique-Eingangs-FPS nach der `60->30`-Duplikatfilterung (fällt auf `0.0` nach 1.5 s Timeout).
    pub fn measured_fps(&self) -> f32 {
        let Some(last_ts) = self.last_unique_frame_instant else {
            return 0.0_f32;
        };
        if last_ts.elapsed() > Duration::from_millis(1500) {
            return 0.0_f32;
        }
        if self.fps_initialized && self.smoothed_frame_interval_ms > 0.1_f32 {
            1000.0_f32 / self.smoothed_frame_interval_ms
        } else {
            0.0_f32
        }
    }

    /// Gleitender Anteil erkannter Duplikat-Frames in Prozent (`0.0 ..= 100.0`).
    pub fn duplicate_ratio_percent(&self) -> f32 {
        if self.has_live_signal() {
            self.smoothed_duplicate_pct.clamp(0.0_f32, 100.0_f32)
        } else {
            0.0_f32
        }
    }

    pub fn active_resolution(&self) -> Option<(u32, u32)> {
        self.active_resolution
    }

    pub fn active_format_label(&self) -> &str {
        &self.active_format_label
    }

    pub fn active_config(&self) -> Option<&CaptureConfig> {
        self.active_config.as_ref()
    }

    #[allow(dead_code)]
    pub fn received_frames(&self) -> u64 {
        self.received_frames
    }

    pub fn dropped_duplicates(&self) -> u64 {
        self.dropped_duplicates
    }

    fn drain_worker_events(&mut self, logs: &mut LogStore) {
        let Some(rx) = self.event_rx.clone() else {
            return;
        };
        let lang = detect_system_language();

        while let Ok(ev) = rx.try_recv() {
            match ev {
                CaptureWorkerEvent::Info(msg) => logs.info(msg),
                CaptureWorkerEvent::Warn(msg) => logs.warn(msg),
                CaptureWorkerEvent::Error(msg) => logs.error(msg),
                CaptureWorkerEvent::StreamOpened {
                    width,
                    height,
                    fps,
                    format_label,
                } => {
                    self.active_resolution = Some((width, height));
                    self.active_format_label = format_label.clone();
                    logs.info(format!(
                        "{}: {}x{} @ {} FPS ({}: {})",
                        tr(
                            lang,
                            "[Capture] Live-Stream aktiv",
                            "[Capture] Live stream active"
                        ),
                        width,
                        height,
                        fps,
                        tr(lang, "Hardware-Format", "Hardware format"),
                        format_label
                    ));
                }
                CaptureWorkerEvent::StreamStopped => {
                    self.has_signal = false;
                }
            }
        }
    }
}

impl Drop for CaptureManager {
    fn drop(&mut self) {
        if let Some(flag) = self.running.take() {
            flag.store(false, Ordering::SeqCst);
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Dedizierte Worker-Schleife: Initialisiert COM im MTA-Modus, handelt das latenzärmste Format aus,
/// dekodiert nach RGBA8, prüft auf 60->30-Duplikate und schiebt immer das frischeste Frame in den Kanal.
fn run_capture_worker(
    config: CaptureConfig,
    running: Arc<AtomicBool>,
    frame_tx: Sender<CapturedFrame>,
    frame_drop_rx: Receiver<CapturedFrame>,
    event_tx: Sender<CaptureWorkerEvent>,
) {
    let _com = ComApartmentGuard::new_multithreaded();
    ensure_nokhwa_initialized();
    let lang = detect_system_language();

    let mut camera = match open_low_latency_camera(&config, &event_tx) {
        Ok(cam) => cam,
        Err(err) => {
            let _ = event_tx.send(CaptureWorkerEvent::Error(format!(
                "[Capture] '{}' {}: {err:#}",
                config.device_name,
                tr(lang, "konnte nicht geöffnet werden", "failed to open")
            )));
            let _ = event_tx.send(CaptureWorkerEvent::StreamStopped);
            return;
        }
    };

    if let Err(err) = camera.open_stream() {
        let _ = event_tx.send(CaptureWorkerEvent::Error(format!(
            "{}: {err}",
            tr(
                lang,
                "[Capture] Starten des Video-Streams fehlgeschlagen",
                "[Capture] Failed to start video stream"
            )
        )));
        let _ = event_tx.send(CaptureWorkerEvent::StreamStopped);
        return;
    }

    let negotiated = camera.camera_format();
    let _ = event_tx.send(CaptureWorkerEvent::StreamOpened {
        width: negotiated.resolution().width(),
        height: negotiated.resolution().height(),
        fps: negotiated.frame_rate(),
        format_label: format!("{}", negotiated.format()),
    });

    let mut frame_index: u64 = 0;
    let mut consecutive_errors: u32 = 0;
    let mut prev_sample_rgb: Vec<u8> = Vec::with_capacity(DUPLICATE_SAMPLE_COUNT * 3);
    let mut prev_dims: (u32, u32) = (0, 0);
    let mut prev_was_duplicate = false;

    while running.load(Ordering::Relaxed) {
        let buffer = match camera.frame() {
            Ok(buf) => {
                consecutive_errors = 0;
                buf
            }
            Err(err) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors % 25 == 0 {
                    let _ = event_tx.send(CaptureWorkerEvent::Warn(format!(
                        "{} (#{consecutive_errors}): {err}",
                        tr(
                            lang,
                            "[Capture] Frame-Abruf verzögert/fehlgeschlagen",
                            "[Capture] Frame read delayed/failed"
                        )
                    )));
                }
                if consecutive_errors >= 60 {
                    let _ = event_tx.send(CaptureWorkerEvent::Error(String::from(tr(
                        lang,
                        "[Capture] Zu viele aufeinanderfolgende Lesefehler - Capture-Stream beendet.",
                        "[Capture] Too many consecutive read errors - capture stream terminated.",
                    ))));
                    break;
                }
                thread::sleep(Duration::from_millis(25));
                continue;
            }
        };

        if !running.load(Ordering::Relaxed) {
            break;
        }

        let timestamp = Instant::now();
        let res = buffer.resolution();
        let width = res.width();
        let height = res.height();
        if width == 0 || height == 0 {
            continue;
        }

        let source_format = format!("{}", buffer.source_frame_format());
        let expected_bytes = (width as usize) * (height as usize) * 4;
        let mut rgba_data = vec![0u8; expected_bytes];

        if buffer
            .decode_image_to_buffer::<RgbAFormat>(&mut rgba_data)
            .is_err()
        {
            match buffer.decode_image::<RgbAFormat>() {
                Ok(img) => {
                    let raw = img.into_raw();
                    if raw.len() == expected_bytes {
                        rgba_data = raw;
                    } else {
                        continue;
                    }
                }
                Err(decode_err) => {
                    if frame_index == 0 {
                        let _ = event_tx.send(CaptureWorkerEvent::Warn(format!(
                            "{} ({source_format}): {decode_err}",
                            tr(
                                lang,
                                "[Capture] RGBA-Dekodierung fehlgeschlagen",
                                "[Capture] RGBA decoding failed"
                            )
                        )));
                    }
                    continue;
                }
            }
        }

        // Schnelle Stride-Stichprobe für die 60->30-Duplikat-Erkennung:
        // Verhindert zudem, dass zwei aufeinanderfolgende Frames beide als Duplikat markiert werden,
        // damit auch statische Konsolen-Menüs zuverlässig mit 30 FPS weiterlaufen.
        let current_sample = extract_sparse_rgb_samples(&rgba_data);
        let is_duplicate = if prev_dims == (width, height)
            && prev_sample_rgb.len() == current_sample.len()
            && !prev_was_duplicate
        {
            samples_indicate_duplicate(&prev_sample_rgb, &current_sample)
        } else {
            false
        };

        prev_sample_rgb = current_sample;
        prev_dims = (width, height);
        prev_was_duplicate = is_duplicate;

        let captured = CapturedFrame {
            rgba_data,
            width,
            height,
            frame_index,
            timestamp,
            is_duplicate,
            source_format,
        };
        frame_index = frame_index.wrapping_add(1);

        // Zero-Backlog / Drop-Oldest-Strategie: Ist Slot 1 noch nicht von der Render-Schleife
        // abgeholt worden, wird das veraltete Frame sofort verworfen und das neue eingestellt.
        match frame_tx.try_send(captured) {
            Ok(()) => {}
            Err(TrySendError::Full(fresh_frame)) => {
                let _ = frame_drop_rx.try_recv();
                let _ = frame_tx.try_send(fresh_frame);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }

    let _ = camera.stop_stream();
    let _ = event_tx.send(CaptureWorkerEvent::StreamStopped);
}

/// Öffnet die Kamera über Media Foundation und wählt bevorzugt unkomprimierte Formate (`NV12` / `YUYV`)
/// vor komprimiertem `MJPEG`, um die USB-Dekodierlatenz auf der Capture-Card zu minimieren.
fn open_low_latency_camera(
    config: &CaptureConfig,
    event_tx: &Sender<CaptureWorkerEvent>,
) -> Result<Camera> {
    let lang = detect_system_language();
    let (target_w, target_h) = match config.target_resolution {
        TargetResolution::Auto => (1920u32, 1080u32),
        TargetResolution::P1080 => (1920u32, 1080u32),
        TargetResolution::P720 => (1280u32, 720u32),
    };
    let target_fps: u32 = match config.input_fps {
        InputFps::Fps60 => 60,
        InputFps::Fps30 => 30,
    };

    // Stufenweise Anfrage: Zuerst unkomprimiertes NV12 / YUYV per Closest anfragen,
    // danach MJPEG und schließlich AbsoluteHighestFrameRate als universellen Fallback.
    let preferred_fourccs = [FrameFormat::NV12, FrameFormat::YUYV, FrameFormat::MJPEG];
    let mut opened_camera: Option<Camera> = None;

    for fourcc in preferred_fourccs {
        let desired = CameraFormat::new(Resolution::new(target_w, target_h), fourcc, target_fps);
        let req = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(desired));
        if let Ok(cam) =
            Camera::with_backend(config.device_index.clone(), req, ApiBackend::MediaFoundation)
        {
            opened_camera = Some(cam);
            break;
        }
    }

    let mut camera = match opened_camera {
        Some(cam) => cam,
        None => {
            let fallback_req =
                RequestedFormat::new::<RgbAFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
            Camera::with_backend(
                config.device_index.clone(),
                fallback_req,
                ApiBackend::MediaFoundation,
            )
            .context(tr(
                lang,
                "Media Foundation konnte kein kompatibles Kamera-Format initialisieren",
                "Media Foundation could not initialize a compatible camera format",
            ))?
        }
    };

    // Wenn der Treiber seine Formatliste offenlegt, wählen wir gezielt das latenzärmste Profil
    // (NV12/YUYV vor MJPEG bei passender Ziel-Auflösung & Ziel-FPS) aus.
    if let Ok(compatible_formats) = camera.compatible_camera_formats() {
        if let Some(best_format) = select_optimal_hardware_format(
            &compatible_formats,
            config.target_resolution,
            target_w,
            target_h,
            target_fps,
        ) {
            if best_format != camera.camera_format() {
                let exact_req =
                    RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Exact(best_format));
                if camera.set_camera_requset(exact_req).is_ok() {
                    let _ = event_tx.send(CaptureWorkerEvent::Info(format!(
                        "{}: {}x{} @ {} FPS ({})",
                        tr(
                            lang,
                            "[Capture] Niedriglatenz-Format ausgehandelt",
                            "[Capture] Negotiated low-latency format"
                        ),
                        best_format.resolution().width(),
                        best_format.resolution().height(),
                        best_format.frame_rate(),
                        best_format.format()
                    )));
                }
            }
        }
    }

    Ok(camera)
}

/// Bewertet alle von der Capture-Card gemeldeten `CameraFormat`-Profile nach:
/// 1. Auflösungs-Treffer, 2. Ziel-FPS-Treffer, 3. Unkomprimiertem Pixel-Layout (`NV12`/`YUYV` > `MJPEG`).
fn select_optimal_hardware_format(
    formats: &[CameraFormat],
    res_mode: TargetResolution,
    target_w: u32,
    target_h: u32,
    target_fps: u32,
) -> Option<CameraFormat> {
    formats
        .iter()
        .copied()
        .max_by_key(|fmt| score_camera_format(*fmt, res_mode, target_w, target_h, target_fps))
}

fn score_camera_format(
    fmt: CameraFormat,
    res_mode: TargetResolution,
    target_w: u32,
    target_h: u32,
    target_fps: u32,
) -> i64 {
    let w = fmt.resolution().width();
    let h = fmt.resolution().height();
    let fps = fmt.frame_rate();

    if w == 0 || h == 0 || fps == 0 {
        return i64::MIN;
    }

    // 1. Format-Latenz-Score: Unkomprimiertes YUV/RGB vermeidet ~8-18 ms MJPEG-Kompressionslatenz!
    let format_score: i64 = match fmt.format() {
        FrameFormat::NV12 => 4_500,
        FrameFormat::YUYV => 4_400,
        FrameFormat::MJPEG => 1_500,
        FrameFormat::GRAY => -20_000,
        _ => 4_200,
    };

    // 2. Auflösungs-Score
    let res_score: i64 = match res_mode {
        TargetResolution::Auto => {
            // Im Auto-Modus ist 1920x1080 der ideale HDMI-Sweetspot, gefolgt von 1440p und 720p.
            if w == 1920 && h == 1080 {
                30_000
            } else if w == 2560 && h == 1440 {
                26_000
            } else if w == 1280 && h == 720 {
                22_000
            } else {
                let diff =
                    (w as i64 - 1920).abs() * 8 + (h as i64 - 1080).abs() * 8;
                15_000 - diff
            }
        }
        TargetResolution::P1080 | TargetResolution::P720 => {
            if w == target_w && h == target_h {
                40_000
            } else {
                let diff = (w as i64 - target_w as i64).abs() * 12
                    + (h as i64 - target_h as i64).abs() * 12;
                20_000 - diff
            }
        }
    };

    // 3. Bildraten-Score
    let fps_score: i64 = if fps == target_fps {
        25_000
    } else if fps > target_fps {
        18_000 - ((fps - target_fps) as i64 * 120)
    } else {
        // Niedrigere FPS als angefordert stark abstrafen (z. B. 1080p5 oder 1080p30 bei 60-FPS-Wunsch).
        10_000 - ((target_fps - fps) as i64 * 600)
    };

    res_score + fps_score + format_score
}

/// Extrahiert gleichmäßig über das gesamte Bild verteilte RGB-Tripel für den schnellen Vorframe-Vergleich.
fn extract_sparse_rgb_samples(rgba: &[u8]) -> Vec<u8> {
    let pixel_count = rgba.len() / 4;
    if pixel_count == 0 {
        return Vec::new();
    }

    let samples = DUPLICATE_SAMPLE_COUNT.min(pixel_count);
    let stride = (pixel_count / samples).max(1);
    let mut out = Vec::with_capacity(samples * 3);

    for i in 0..samples {
        let px_idx = (i * stride).min(pixel_count - 1) * 4;
        out.push(rgba[px_idx]);
        out.push(rgba[px_idx + 1]);
        out.push(rgba[px_idx + 2]);
    }

    out
}

/// Vergleicht zwei RGB-Stichprobenvektoren fehlertolerant gegenüber minimalem HDMI-Quantisierungsrauschen (+/-1 LSB).
fn samples_indicate_duplicate(prev: &[u8], curr: &[u8]) -> bool {
    if prev.is_empty() || prev.len() != curr.len() {
        return false;
    }

    let mut total_abs_diff: u32 = 0;
    let mut max_channel_diff: u8 = 0;

    for (&a, &b) in prev.iter().zip(curr.iter()) {
        let diff = a.abs_diff(b);
        total_abs_diff += diff as u32;
        if diff > max_channel_diff {
            max_channel_diff = diff;
        }
    }

    let mean_diff = (total_abs_diff as f32) / (curr.len() as f32);
    max_channel_diff <= 6 && mean_diff <= 1.15_f32
}
