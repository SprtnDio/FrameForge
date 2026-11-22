use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig, StreamError};
use crossbeam_channel::{unbounded, Receiver, Sender};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::ui::LogStore;

/// Ziel-Ringpuffer-Füllstand (~20 ms) für minimale hörbare Latenz ohne Underruns.
const TARGET_BUFFER_LATENCY_MS: f32 = 20.0_f32;
/// Sanfte Start-Vorfüllung (~16 ms), damit der erste WASAPI-Output-Callback sofort Material hat.
const INITIAL_PREFILL_LATENCY_MS: f32 = 16.0_f32;
/// Harte Drift-Obergrenze (~44 ms) mit weichem Crossfade-Trimming bei Quarz-Taktunterschieden.
const MAX_DRIFT_LATENCY_MS: f32 = 44.0_f32;
/// Crossfade-Fenster (in Stereo-Frames) beim Drift-Trimming gegen Knackser.
const DRIFT_CROSSFADE_FRAMES: usize = 32;

struct AudioComGuard {
  should_uninitialize: bool,
}

impl AudioComGuard {
  fn new_multithreaded() -> Self {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    Self { should_uninitialize: hr.is_ok() }
  }
}

impl Drop for AudioComGuard {
  fn drop(&mut self) {
    if self.should_uninitialize {
      unsafe { CoUninitialize(); }
    }
  }
}

/// Beschreibt ein über WASAPI (`cpal`) erkanntes Audio-Ein- oder Ausgabegerät.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
  pub raw_name: String,
  pub display_name: String,
}

/// Gewünschte Routing-Konfiguration für den Hintergrund-Audio-Supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRouteConfig {
  pub input_device_name: String,
  pub output_device_name: Option<String>,
}

/// Live-Metriken der Audio-Engine für UI-Pegelanzeige und das Performance-Overlay (OSD).
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioLiveTelemetry {
  pub enabled: bool,
  pub muted: bool,
  pub active_stream_ok: bool,
  pub volume: f32,
  pub current_peak_l: f32,
  pub current_peak_r: f32,
  pub buffer_latency_ms: f32,
  pub sample_rate_hz: u32,
}

struct AudioSharedAtomics {
  audio_enabled: AtomicBool,
  muted: AtomicBool,
  volume_bits: AtomicU32,
  current_peak_l_bits: AtomicU32,
  current_peak_r_bits: AtomicU32,
  buffer_latency_ms_bits: AtomicU32,
  sample_rate_hz: AtomicU32,
  active_stream_ok: AtomicBool,
}

impl AudioSharedAtomics {
  fn new(enabled: bool, muted: bool, volume: f32) -> Self {
    Self {
      audio_enabled: AtomicBool::new(enabled),
      muted: AtomicBool::new(muted),
      volume_bits: AtomicU32::new(volume.clamp(0.0_f32, 2.0_f32).to_bits()),
      current_peak_l_bits: AtomicU32::new(0.0_f32.to_bits()),
      current_peak_r_bits: AtomicU32::new(0.0_f32.to_bits()),
      buffer_latency_ms_bits: AtomicU32::new(0.0_f32.to_bits()),
      sample_rate_hz: AtomicU32::new(0),
      active_stream_ok: AtomicBool::new(false),
    }
  }

  #[inline]
  fn set_enabled(&self, enabled: bool) { self.audio_enabled.store(enabled, Ordering::Relaxed); }
  #[inline]
  fn is_enabled(&self) -> bool { self.audio_enabled.load(Ordering::Relaxed) }
  #[inline]
  fn set_muted(&self, muted: bool) { self.muted.store(muted, Ordering::Relaxed); }
  #[inline]
  fn is_muted(&self) -> bool { self.muted.load(Ordering::Relaxed) }
  #[inline]
  fn set_volume(&self, volume: f32) { self.volume_bits.store(volume.clamp(0.0_f32, 2.0_f32).to_bits(), Ordering::Relaxed); }
  #[inline]
  fn volume(&self) -> f32 { f32::from_bits(self.volume_bits.load(Ordering::Relaxed)) }

  #[inline]
  fn store_peaks(&self, peak_l: f32, peak_r: f32) {
    let prev_l = f32::from_bits(self.current_peak_l_bits.load(Ordering::Relaxed));
    let prev_r = f32::from_bits(self.current_peak_r_bits.load(Ordering::Relaxed));
    let smooth_l = if peak_l >= prev_l { peak_l } else { prev_l * 0.82_f32 + peak_l * 0.18_f32 };
    let smooth_r = if peak_r >= prev_r { peak_r } else { prev_r * 0.82_f32 + peak_r * 0.18_f32 };
    self.current_peak_l_bits.store(smooth_l.clamp(0.0_f32, 1.5_f32).to_bits(), Ordering::Relaxed);
    self.current_peak_r_bits.store(smooth_r.clamp(0.0_f32, 1.5_f32).to_bits(), Ordering::Relaxed);
  }

  #[inline]
  fn decay_peaks(&self) {
    let prev_l = f32::from_bits(self.current_peak_l_bits.load(Ordering::Relaxed));
    let prev_r = f32::from_bits(self.current_peak_r_bits.load(Ordering::Relaxed));
    let next_l = if prev_l < 0.002_f32 { 0.0_f32 } else { prev_l * 0.72_f32 };
    let next_r = if prev_r < 0.002_f32 { 0.0_f32 } else { prev_r * 0.72_f32 };
    self.current_peak_l_bits.store(next_l.to_bits(), Ordering::Relaxed);
    self.current_peak_r_bits.store(next_r.to_bits(), Ordering::Relaxed);
  }

  #[inline]
  fn reset_runtime_metrics(&self) {
    self.active_stream_ok.store(false, Ordering::Relaxed);
    self.buffer_latency_ms_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    self.current_peak_l_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    self.current_peak_r_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    self.sample_rate_hz.store(0, Ordering::Relaxed);
  }

  fn snapshot(&self) -> AudioLiveTelemetry {
    let enabled = self.is_enabled();
    let muted = self.is_muted();
    let active_stream_ok = enabled && self.active_stream_ok.load(Ordering::Relaxed);
    AudioLiveTelemetry {
      enabled,
      muted,
      active_stream_ok,
      volume: self.volume(),
      current_peak_l: if active_stream_ok && !muted { f32::from_bits(self.current_peak_l_bits.load(Ordering::Relaxed)) } else { 0.0_f32 },
      current_peak_r: if active_stream_ok && !muted { f32::from_bits(self.current_peak_r_bits.load(Ordering::Relaxed)) } else { 0.0_f32 },
      buffer_latency_ms: if active_stream_ok { f32::from_bits(self.buffer_latency_ms_bits.load(Ordering::Relaxed)) } else { 0.0_f32 },
      sample_rate_hz: self.sample_rate_hz.load(Ordering::Relaxed),
    }
  }
}

enum AudioSupervisorCmd {
  ApplyRoute(Option<AudioRouteConfig>),
  Shutdown,
}

enum AudioWorkerEvent {
  Info(String),
  Warn(String),
  Error(String),
}

struct StereoDriftBuffer {
  queue: VecDeque<[f32; 2]>,
  target_frames: usize,
  prefill_frames: usize,
  max_drift_frames: usize,
  primed: bool,
}

impl StereoDriftBuffer {
  fn new(output_sample_rate: u32) -> Self {
    let rate = output_sample_rate.max(8_000) as f32;
    let target_frames = ((rate * TARGET_BUFFER_LATENCY_MS) / 1000.0_f32).round().max(256.0_f32) as usize;
    let prefill_frames = ((rate * INITIAL_PREFILL_LATENCY_MS) / 1000.0_f32).round().max(192.0_f32) as usize;
    let max_drift_frames = ((rate * MAX_DRIFT_LATENCY_MS) / 1000.0_f32).round().max((target_frames + 256) as f32) as usize;
    let capacity = ((rate * 0.20_f32) as usize).max(max_drift_frames * 2);
    Self { queue: VecDeque::with_capacity(capacity), target_frames, prefill_frames, max_drift_frames, primed: false }
  }

  fn clear(&mut self) {
    self.queue.clear();
    self.primed = false;
  }

  fn push_stereo_batch(&mut self, frames: &[[f32; 2]]) {
    for &frame in frames { self.queue.push_back(frame); }
    if !self.primed && self.queue.len() >= self.prefill_frames { self.primed = true; }

    if self.queue.len() > self.max_drift_frames {
      let drop_count = self.queue.len().saturating_sub(self.target_frames);
      if drop_count > 0 {
        let mut tail_before_cut = self.queue.front().copied().unwrap_or([0.0_f32; 2]);
        for _ in 0..drop_count {
          if let Some(popped) = self.queue.pop_front() { tail_before_cut = popped; }
        }
        let fade_len = DRIFT_CROSSFADE_FRAMES.min(self.queue.len());
        for i in 0..fade_len {
          let t = (i as f32) / (fade_len as f32);
          if let Some(slot) = self.queue.get_mut(i) {
            slot[0] = tail_before_cut[0] * (1.0_f32 - t) + slot[0] * t;
            slot[1] = tail_before_cut[1] * (1.0_f32 - t) + slot[1] * t;
          }
        }
      }
    }
  }

  #[inline]
  fn pop_stereo(&mut self) -> [f32; 2] {
    if !self.primed { return [0.0_f32, 0.0_f32]; }
    match self.queue.pop_front() {
      Some(frame) => frame,
      None => {
        self.primed = false;
        [0.0_f32, 0.0_f32]
      }
    }
  }

  #[inline]
  fn len(&self) -> usize { self.queue.len() }
}

struct LinearStereoResampler {
  in_rate: u32,
  out_rate: u32,
  step_in_per_out: f64,
  pos_in_input: f64,
  prev_frame: [f32; 2],
  has_prev: bool,
  scratch_out: Vec<[f32; 2]>,
}

impl LinearStereoResampler {
  fn new(in_rate: u32, out_rate: u32) -> Self {
    let (safe_in, safe_out) = (in_rate.max(1), out_rate.max(1));
    Self {
      in_rate: safe_in,
      out_rate: safe_out,
      step_in_per_out: (safe_in as f64) / (safe_out as f64),
      pos_in_input: 0.0_f64,
      prev_frame: [0.0_f32, 0.0_f32],
      has_prev: false,
      scratch_out: Vec::with_capacity(2048),
    }
  }

  fn process<'a>(&'a mut self, input_stereo: &[[f32; 2]]) -> &'a [[f32; 2]] {
    self.scratch_out.clear();
    if input_stereo.is_empty() { return &self.scratch_out; }

    if self.in_rate == self.out_rate {
      self.scratch_out.extend_from_slice(input_stereo);
      if let Some(&last) = input_stereo.last() {
        self.prev_frame = last;
        self.has_prev = true;
      }
      return &self.scratch_out;
    }

    if !self.has_prev {
      self.prev_frame = input_stereo[0];
      self.has_prev = true;
    }

    let n = input_stereo.len();
    let mut pos = self.pos_in_input;
    while pos < (n as f64) {
      let shifted = pos + 1.0_f64;
      let base_idx = shifted.floor() as usize;
      let frac = (shifted - (base_idx as f64)) as f32;
      let s0 = if base_idx == 0 { self.prev_frame } else { input_stereo[(base_idx - 1).min(n - 1)] };
      let s1 = if base_idx >= n { input_stereo[n - 1] } else { input_stereo[base_idx] };
      self.scratch_out.push([s0[0] + (s1[0] - s0[0]) * frac, s0[1] + (s1[1] - s0[1]) * frac]);
      pos += self.step_in_per_out;
    }

    self.pos_in_input = (pos - (n as f64)).max(-1.0_f64);
    self.prev_frame = input_stereo[n - 1];
    &self.scratch_out
  }
}

pub fn list_audio_input_devices(logs: &mut LogStore) -> Vec<AudioDeviceInfo> {
  let _com = AudioComGuard::new_multithreaded();
  let host = cpal::default_host();
  let mut out = Vec::new();
  match host.input_devices() {
    Ok(devices) => {
      for dev in devices {
        if let Ok(raw_name) = dev.name() {
          let display_name = sanitize_audio_label(&raw_name);
          if !display_name.is_empty() { out.push(AudioDeviceInfo { raw_name, display_name }); }
        }
      }
      logs.info(format!("[Audio] WASAPI-Scan: {} Audio-Eingabegerät(e) gefunden.", out.len()));
    }
    Err(err) => logs.warn(format!("[Audio] WASAPI-Eingabegeräte konnten nicht abgefragt werden: {err}")),
  }
  out
}

pub fn list_audio_output_devices(logs: &mut LogStore) -> Vec<AudioDeviceInfo> {
  let _com = AudioComGuard::new_multithreaded();
  let host = cpal::default_host();
  let mut out = Vec::new();
  match host.output_devices() {
    Ok(devices) => {
      for dev in devices {
        if let Ok(raw_name) = dev.name() {
          let display_name = sanitize_audio_label(&raw_name);
          if !display_name.is_empty() { out.push(AudioDeviceInfo { raw_name, display_name }); }
        }
      }
      logs.info(format!("[Audio] WASAPI-Scan: {} Audio-Ausgabegerät(e) gefunden.", out.len()));
    }
    Err(err) => logs.warn(format!("[Audio] WASAPI-Ausgabegeräte konnten nicht abgefragt werden: {err}")),
  }
  out
}

pub fn find_preferred_capture_audio_index(devices: &[AudioDeviceInfo]) -> Option<usize> {
  let mut best_idx: Option<usize> = None;
  let mut best_score: i32 = 0;

  for (idx, dev) in devices.iter().enumerate() {
    let lower = dev.raw_name.to_lowercase();
    let mut score: i32 = 0;
    if lower.contains("capture") { score += 50; }
    if lower.contains("hdmi") { score += 45; }
    if lower.contains("digital audio") || lower.contains("digitale audioschnittstelle") { score += 42; }
    if lower.contains("ms21")
      || lower.contains("elgato")
      || lower.contains("avermedia")
      || lower.contains("cam link")
      || lower.contains("mirabox")
      || lower.contains("ugreen")
      || lower.contains("genki")
    {
      score += 55;
    }
    if lower.contains("video") { score += 28; }
    if lower.contains("usb") { score += 12; }
    if lower.contains("microphone array") || lower.contains("mikrofonarray") { score -= 25; }

    if score > best_score {
      best_score = score;
      best_idx = Some(idx);
    }
  }
  best_idx
}

fn sanitize_audio_label(raw: &str) -> String {
  raw
    .chars()
    .filter(|&c| c.is_ascii_graphic() || c == ' ' || matches!(c, 'ä' | 'ö' | 'ü' | 'Ä' | 'Ö' | 'Ü' | 'ß'))
    .collect::<String>()
    .trim()
    .to_string()
}

/// Steuert den dedizierten Audio-Supervisor-Thread (`cpal::Stream` bleibt als `!Send` lokal im Worker).
pub struct AudioManager {
  cmd_tx: Option<Sender<AudioSupervisorCmd>>,
  event_rx: Option<Receiver<AudioWorkerEvent>>,
  supervisor_handle: Option<JoinHandle<()>>,
  atomics: Arc<AudioSharedAtomics>,
  active_route: Option<AudioRouteConfig>,
}

impl AudioManager {
  pub fn new(enabled: bool, muted: bool, volume: f32) -> Self {
    let atomics = Arc::new(AudioSharedAtomics::new(enabled, muted, volume));
    let worker_atomics = Arc::clone(&atomics);
    let (cmd_tx, cmd_rx) = unbounded::<AudioSupervisorCmd>();
    let (event_tx, event_rx) = unbounded::<AudioWorkerEvent>();

    let handle = thread::Builder::new()
      .name("frameforge-audio".to_string())
      .spawn(move || run_audio_supervisor(cmd_rx, event_tx, worker_atomics))
      .ok();

    Self {
      cmd_tx: Some(cmd_tx),
      event_rx: Some(event_rx),
      supervisor_handle: handle,
      atomics,
      active_route: None,
    }
  }

  pub fn sync_settings(&mut self, enabled: bool, muted: bool, volume: f32, desired_route: Option<AudioRouteConfig>) {
    self.atomics.set_enabled(enabled);
    self.atomics.set_muted(muted);
    self.atomics.set_volume(volume);

    let effective_route = if enabled { desired_route } else { None };
    if self.active_route != effective_route {
      self.active_route = effective_route.clone();
      if let Some(tx) = &self.cmd_tx {
        let _ = tx.send(AudioSupervisorCmd::ApplyRoute(effective_route));
      }
    }
  }

  pub fn poll_events(&mut self, logs: &mut LogStore) {
    let Some(rx) = &self.event_rx else { return; };
    while let Ok(ev) = rx.try_recv() {
      match ev {
        AudioWorkerEvent::Info(msg) => logs.info(msg),
        AudioWorkerEvent::Warn(msg) => logs.warn(msg),
        AudioWorkerEvent::Error(msg) => logs.error(msg),
      }
    }
  }

  pub fn telemetry(&self) -> AudioLiveTelemetry { self.atomics.snapshot() }

  pub fn shutdown(&mut self, logs: &mut LogStore) {
    if let Some(tx) = self.cmd_tx.take() { let _ = tx.send(AudioSupervisorCmd::Shutdown); }
    if let Some(handle) = self.supervisor_handle.take() { let _ = handle.join(); }
    self.poll_events(logs);
    self.active_route = None;
    self.atomics.reset_runtime_metrics();
  }
}

impl Drop for AudioManager {
  fn drop(&mut self) {
    if let Some(tx) = self.cmd_tx.take() { let _ = tx.send(AudioSupervisorCmd::Shutdown); }
    if let Some(handle) = self.supervisor_handle.take() { let _ = handle.join(); }
  }
}

fn run_audio_supervisor(cmd_rx: Receiver<AudioSupervisorCmd>, event_tx: Sender<AudioWorkerEvent>, atomics: Arc<AudioSharedAtomics>) {
  let _com = AudioComGuard::new_multithreaded();
  let mut active_streams: Option<(Stream, Stream)> = None;

  loop {
    match cmd_rx.recv_timeout(Duration::from_millis(80)) {
      Ok(AudioSupervisorCmd::ApplyRoute(route_opt)) => {
        active_streams = None;
        atomics.reset_runtime_metrics();
        if let Some(route) = route_opt {
          match build_and_start_bridge(&route, &atomics, &event_tx) {
            Ok((in_stream, out_stream)) => {
              atomics.active_stream_ok.store(true, Ordering::Relaxed);
              active_streams = Some((in_stream, out_stream));
            }
            Err(err) => {
              atomics.reset_runtime_metrics();
              let _ = event_tx.send(AudioWorkerEvent::Error(format!("[Audio] Start der HDMI-Audio-Bridge fehlgeschlagen: {err:#}")));
            }
          }
        } else {
          let _ = event_tx.send(AudioWorkerEvent::Info("[Audio] Audio-Passthrough pausiert.".to_string()));
        }
      }
      Ok(AudioSupervisorCmd::Shutdown) => {
        drop(active_streams);
        atomics.reset_runtime_metrics();
        break;
      }
      Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
        if active_streams.is_none() || !atomics.is_enabled() || atomics.is_muted() {
          atomics.decay_peaks();
        }
      }
      Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
        drop(active_streams);
        break;
      }
    }
  }
}

fn build_and_start_bridge(route: &AudioRouteConfig, atomics: &Arc<AudioSharedAtomics>, event_tx: &Sender<AudioWorkerEvent>) -> Result<(Stream, Stream)> {
  let host = cpal::default_host();
  let input_device = find_input_device_by_name(&host, &route.input_device_name)
    .with_context(|| format!("Audio-Eingabegerät '{}' nicht gefunden", route.input_device_name))?;

  let (output_device, output_label) = match &route.output_device_name {
    Some(target_name) => match find_output_device_by_name(&host, target_name) {
      Some(dev) => (dev, sanitize_audio_label(target_name)),
      None => {
        let _ = event_tx.send(AudioWorkerEvent::Warn(format!(
          "[Audio] Ausgabegerät '{}' nicht verfügbar - nutze System-Standard.",
          sanitize_audio_label(target_name)
        )));
        let def = host.default_output_device().context("Kein Standard-Audio-Ausgabegerät unter Windows gefunden")?;
        let name = def.name().map(|n| sanitize_audio_label(&n)).unwrap_or_else(|_| String::from("System-Standard"));
        (def, name)
      }
    },
    None => {
      let def = host.default_output_device().context("Kein Standard-Audio-Ausgabegerät unter Windows gefunden")?;
      let name = def.name().map(|n| sanitize_audio_label(&n)).unwrap_or_else(|_| String::from("System-Standard"));
      (def, name)
    }
  };

  let supported_in = input_device.default_input_config().context("Abfrage von default_input_config() fehlgeschlagen")?;
  let supported_out = output_device.default_output_config().context("Abfrage von default_output_config() fehlgeschlagen")?;
  let (in_format, out_format) = (supported_in.sample_format(), supported_out.sample_format());
  let (in_config, out_config): (StreamConfig, StreamConfig) = (supported_in.into(), supported_out.into());
  let (in_channels, out_channels) = ((in_config.channels as usize).max(1), (out_config.channels as usize).max(1));
  let (in_rate, out_rate) = (in_config.sample_rate.0.max(8_000), out_config.sample_rate.0.max(8_000));

  atomics.sample_rate_hz.store(out_rate, Ordering::Relaxed);
  let ring = Arc::new(Mutex::new(StereoDriftBuffer::new(out_rate)));

  let input_stream = build_typed_input_stream(&input_device, &in_config, in_format, in_channels, in_rate, out_rate, Arc::clone(&ring), Arc::clone(atomics), event_tx.clone())?;
  let output_stream = build_typed_output_stream(&output_device, &out_config, out_format, out_channels, out_rate, ring, Arc::clone(atomics), event_tx.clone())?;

  output_stream.play().context("Starten des WASAPI-Output-Streams fehlgeschlagen")?;
  input_stream.play().context("Starten des WASAPI-Input-Streams fehlgeschlagen")?;

  let input_label = sanitize_audio_label(&route.input_device_name);
  let resample_note = if in_rate == out_rate { format!("{} Hz nativ", out_rate) } else { format!("{} Hz -> {} Hz Linear-Resampling", in_rate, out_rate) };
  let _ = event_tx.send(AudioWorkerEvent::Info(format!(
    "[Audio] Bridge aktiv: '{}' ({}ch {:?}) -> '{}' ({}ch {:?}, {})",
    input_label, in_channels, in_format, output_label, out_channels, out_format, resample_note
  )));

  Ok((input_stream, output_stream))
}

fn find_input_device_by_name(host: &cpal::Host, target_raw_name: &str) -> Option<Device> {
  for dev in host.input_devices().ok()? {
    if dev.name().ok().as_deref() == Some(target_raw_name) { return Some(dev); }
  }
  None
}

fn find_output_device_by_name(host: &cpal::Host, target_raw_name: &str) -> Option<Device> {
  for dev in host.output_devices().ok()? {
    if dev.name().ok().as_deref() == Some(target_raw_name) { return Some(dev); }
  }
  None
}

#[allow(clippy::too_many_arguments)]
fn build_typed_input_stream(
  device: &Device, config: &StreamConfig, sample_format: SampleFormat, in_channels: usize,
  in_rate: u32, out_rate: u32, ring: Arc<Mutex<StereoDriftBuffer>>,
  atomics: Arc<AudioSharedAtomics>, event_tx: Sender<AudioWorkerEvent>,
) -> Result<Stream> {
  let err_atomics = Arc::clone(&atomics);
  let err_fn = move |err: StreamError| {
    err_atomics.active_stream_ok.store(false, Ordering::Relaxed);
    let _ = event_tx.send(AudioWorkerEvent::Warn(format!("[Audio] Eingabe-Stream-Warnung: {err}")));
  };

  let mut scratch_in: Vec<[f32; 2]> = Vec::with_capacity(2048);
  let mut resampler = LinearStereoResampler::new(in_rate, out_rate);

  match sample_format {
    SampleFormat::F32 => device
      .build_input_stream(config, move |data: &[f32], _: &cpal::InputCallbackInfo| {
        handle_input_samples(data, in_channels, |s| s, &mut scratch_in, &mut resampler, &ring, &atomics);
      }, err_fn, None)
      .context("build_input_stream (F32) fehlgeschlagen"),
    SampleFormat::I16 => device
      .build_input_stream(config, move |data: &[i16], _: &cpal::InputCallbackInfo| {
        handle_input_samples(data, in_channels, |s| (s as f32) / 32768.0_f32, &mut scratch_in, &mut resampler, &ring, &atomics);
      }, err_fn, None)
      .context("build_input_stream (I16) fehlgeschlagen"),
    SampleFormat::U16 => device
      .build_input_stream(config, move |data: &[u16], _: &cpal::InputCallbackInfo| {
        handle_input_samples(data, in_channels, |s| ((s as f32) - 32768.0_f32) / 32768.0_f32, &mut scratch_in, &mut resampler, &ring, &atomics);
      }, err_fn, None)
      .context("build_input_stream (U16) fehlgeschlagen"),
    other => bail!("Nicht unterstütztes Eingangs-SampleFormat: {other:?}"),
  }
}

fn handle_input_samples<T: Copy>(
  interleaved: &[T], in_channels: usize, to_f32: impl Fn(T) -> f32,
  scratch_in: &mut Vec<[f32; 2]>, resampler: &mut LinearStereoResampler,
  ring: &Arc<Mutex<StereoDriftBuffer>>, atomics: &Arc<AudioSharedAtomics>,
) {
  if !atomics.is_enabled() {
    if let Ok(mut guard) = ring.try_lock() { guard.clear(); }
    atomics.decay_peaks();
    return;
  }

  scratch_in.clear();
  let (mut peak_l, mut peak_r) = (0.0_f32, 0.0_f32);
  let gain_preview = if atomics.is_muted() { 0.0_f32 } else { atomics.volume() };

  for frame in interleaved.chunks_exact(in_channels) {
    let (l, r) = if in_channels == 1 {
      let m = to_f32(frame[0]);
      (m, m)
    } else {
      (to_f32(frame[0]), to_f32(frame[1]))
    };
    peak_l = peak_l.max((l * gain_preview).abs());
    peak_r = peak_r.max((r * gain_preview).abs());
    scratch_in.push([l, r]);
  }

  atomics.store_peaks(peak_l.min(1.0_f32), peak_r.min(1.0_f32));
  let resampled = resampler.process(scratch_in);
  if let Ok(mut guard) = ring.lock() { guard.push_stereo_batch(resampled); }
}

#[allow(clippy::too_many_arguments)]
fn build_typed_output_stream(
  device: &Device, config: &StreamConfig, sample_format: SampleFormat, out_channels: usize,
  out_rate: u32, ring: Arc<Mutex<StereoDriftBuffer>>,
  atomics: Arc<AudioSharedAtomics>, event_tx: Sender<AudioWorkerEvent>,
) -> Result<Stream> {
  let err_atomics = Arc::clone(&atomics);
  let err_fn = move |err: StreamError| {
    err_atomics.active_stream_ok.store(false, Ordering::Relaxed);
    let _ = event_tx.send(AudioWorkerEvent::Warn(format!("[Audio] Ausgabe-Stream-Warnung: {err}")));
  };

  match sample_format {
    SampleFormat::F32 => device
      .build_output_stream(config, move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        write_output_samples(data, out_channels, out_rate, 0.0_f32, |s| s, &ring, &atomics);
      }, err_fn, None)
      .context("build_output_stream (F32) fehlgeschlagen"),
    SampleFormat::I16 => device
      .build_output_stream(config, move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
        write_output_samples(data, out_channels, out_rate, 0i16, |s| (s.clamp(-1.0_f32, 1.0_f32) * 32767.0_f32) as i16, &ring, &atomics);
      }, err_fn, None)
      .context("build_output_stream (I16) fehlgeschlagen"),
    SampleFormat::U16 => device
      .build_output_stream(config, move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
        write_output_samples(data, out_channels, out_rate, 32768u16, |s| ((s.clamp(-1.0_f32, 1.0_f32) * 0.5_f32 + 0.5_f32) * 65535.0_f32) as u16, &ring, &atomics);
      }, err_fn, None)
      .context("build_output_stream (U16) fehlgeschlagen"),
    other => bail!("Nicht unterstütztes Ausgangs-SampleFormat: {other:?}"),
  }
}

fn write_output_samples<T: Copy>(
  out_interleaved: &mut [T], out_channels: usize, out_rate: u32, silence: T,
  from_f32: impl Fn(f32) -> T, ring: &Arc<Mutex<StereoDriftBuffer>>, atomics: &Arc<AudioSharedAtomics>,
) {
  let enabled = atomics.is_enabled();
  let muted = atomics.is_muted();
  let volume = atomics.volume();

  if !enabled {
    out_interleaved.fill(silence);
    atomics.buffer_latency_ms_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    return;
  }

  let Ok(mut guard) = ring.lock() else {
    out_interleaved.fill(silence);
    return;
  };

  for frame in out_interleaved.chunks_exact_mut(out_channels) {
    let [raw_l, raw_r] = guard.pop_stereo();
    if muted || volume <= 0.0001_f32 {
      frame.fill(silence);
      continue;
    }
    let l = apply_gain_with_soft_clip(raw_l, volume);
    let r = apply_gain_with_soft_clip(raw_r, volume);
    if out_channels == 1 {
      frame[0] = from_f32(0.5_f32 * (l + r));
    } else {
      frame[0] = from_f32(l);
      frame[1] = from_f32(r);
      for extra in &mut frame[2..] { *extra = silence; }
    }
  }

  let remaining_frames = guard.len();
  drop(guard);
  let latency_ms = ((remaining_frames as f32) * 1000.0_f32) / (out_rate.max(1) as f32);
  atomics.buffer_latency_ms_bits.store(latency_ms.to_bits(), Ordering::Relaxed);
}

#[inline]
fn apply_gain_with_soft_clip(sample: f32, volume: f32) -> f32 {
  let scaled = sample * volume;
  if volume > 1.0_f32 || scaled.abs() > 0.98_f32 { scaled.tanh() } else { scaled.clamp(-1.0_f32, 1.0_f32) }
}
