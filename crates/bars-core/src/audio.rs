//! Microphone capture and sound cues.
//!
//! Capture runs on a dedicated thread that owns the (non-`Send`) cpal stream.
//! Audio is delivered as 16 kHz mono f32, the format every engine expects. On
//! PulseAudio/PipeWire we ask the server for 16 kHz directly; other hosts are
//! resampled locally.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;

pub const SAMPLE_RATE: u32 = 16_000;
const PREROLL: Duration = Duration::from_millis(350);

/// Must run at startup, before any other thread exists.
///
/// Linux: sandboxed (Flatpak) apps can't see `~/.config/pulse/cookie`, so the
/// native PulseAudio client would send an empty cookie, which pipewire-pulse
/// rejects. Like libpulse, fall back to a zeroed 256-byte cookie: PipeWire
/// ignores its contents and PulseAudio authenticates same-user clients by
/// socket credentials.
///
/// macOS: CoreAudio delivers device notifications on its own thread instead
/// of the main run loop (as PortAudio and JUCE configure it), so a device
/// being reconfigured can never wait on the UI thread.
pub fn prepare_environment() {
    #[cfg(target_os = "macos")]
    mac_hal::use_own_notification_thread();
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("PULSE_COOKIE").is_some() {
            return;
        }
        let home = dirs::home_dir().unwrap_or_default();
        if home.join(".config/pulse/cookie").exists() || home.join(".pulse-cookie").exists() {
            return;
        }
        let Some(dir) = dirs::cache_dir().map(|d| d.join(crate::paths::APP_DIR)) else {
            return;
        };
        let path = dir.join("pulse-cookie");
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            if std::fs::write(&path, [0u8; 256]).is_err() {
                return;
            }
        }
        // SAFETY: documented to be called at startup, before threads exist.
        unsafe { std::env::set_var("PULSE_COOKIE", &path) };
    }
}

#[cfg(target_os = "macos")]
mod mac_hal {
    use std::ffi::c_void;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectSetPropertyData(
            object: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: u32,
            data: *const c_void,
        ) -> i32;
    }

    const SYSTEM_OBJECT: u32 = 1; // kAudioObjectSystemObject
    const RUN_LOOP: u32 = u32::from_be_bytes(*b"rnlp"); // kAudioHardwarePropertyRunLoop
    const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");

    /// Sets kAudioHardwarePropertyRunLoop to NULL: the HAL then runs its
    /// own thread for notifications.
    pub fn use_own_notification_thread() {
        let address = AudioObjectPropertyAddress { selector: RUN_LOOP, scope: SCOPE_GLOBAL, element: 0 };
        let run_loop: *const c_void = std::ptr::null();
        let status = unsafe {
            AudioObjectSetPropertyData(
                SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                std::mem::size_of::<*const c_void>() as u32,
                &run_loop as *const *const c_void as *const c_void,
            )
        };
        if status != 0 {
            log::warn!("could not move CoreAudio notifications off the main thread (OSStatus {status})");
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub fn list_input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut out: Vec<InputDevice> = devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let desc = d.description().ok()?;
            let name = desc.name().to_string();
            // Skip loopback "Monitor of ..." sources; they're never a microphone.
            if name.starts_with("Monitor of") || id.ends_with(".monitor") {
                return None;
            }
            Some(InputDevice {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
    out
}

/// A finished recording: mono samples at the microphone's rate (the
/// speech engine converts them to 16 kHz, off the controller's thread).
#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub peak: f32,
}

impl Recording {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / self.rate.max(1) as f64)
    }

    /// RMS of the loudest 100 ms window; a cheap "did anyone speak" signal.
    pub fn loudest_window_rms(&self) -> f32 {
        let win = (self.rate / 10).max(1) as usize;
        self.samples
            .chunks(win)
            .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len().max(1) as f32).sqrt())
            .fold(0.0, f32::max)
    }
}

enum Cmd {
    Start { device: Option<String>, warm: bool },
    /// Open the microphone only to show its level; nothing is recorded.
    Meter { device: Option<String>, on: bool },
    Stop(Sender<Recording>),
    Cancel,
    Warm { device: Option<String>, warm: bool },
    Cue(Cue, f32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cue {
    Start,
    Lock,
    Stop,
    Cancel,
}

struct Shared {
    capturing: bool,
    /// Levels are reported, though nothing is recorded (the microphone
    /// test).
    metering: bool,
    rate: u32,
    channels: u16,
    buf: Vec<f32>,
    preroll: VecDeque<f32>,
    preroll_cap: usize,
    peak: f32,
}

/// Handle to the audio thread. Cheap to clone.
#[derive(Clone)]
pub struct Audio {
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
    levels: LevelSink,
    problems: ProblemSink,
}

pub type LevelSink = Arc<dyn Fn(f32) + Send + Sync>;
/// Told why the microphone couldn't be opened for a recording or a test.
pub type ProblemSink = Arc<dyn Fn(String) + Send + Sync>;

/// How long `Audio::stop` waits for the audio thread.
const STOP_WAIT: Duration = Duration::from_secs(3);

/// The end of a recording.
pub struct Stopped {
    pub recording: Recording,
    /// The audio thread didn't answer: it's stuck in the audio system (a
    /// device that stopped responding). The recording is what the
    /// microphone delivered until then; the thread can't be used again.
    pub stuck: bool,
}

impl Audio {
    /// `levels` receives a smoothed 0..1 input level ~60 times per second
    /// while recording (drives the overlay waveform). `problems` hears when
    /// the microphone couldn't be opened.
    pub fn spawn(levels: LevelSink, problems: ProblemSink) -> Audio {
        let (tx, rx) = unbounded();
        let thread = AudioThread::new(levels.clone(), problems.clone());
        let shared = thread.shared.clone();
        thread::Builder::new()
            .name("bars-audio".into())
            .spawn(move || thread.run(rx))
            .expect("spawn audio thread");
        Audio { tx, shared, levels, problems }
    }

    /// A new audio thread in place of a stuck one, which is left behind.
    pub fn respawn(&self) -> Audio {
        Audio::spawn(self.levels.clone(), self.problems.clone())
    }

    pub fn start(&self, device: Option<String>, warm: bool) {
        let _ = self.tx.send(Cmd::Start { device, warm });
    }

    /// Ends the recording.
    pub fn stop(&self) -> Stopped {
        let (tx, rx) = bounded(1);
        let _ = self.tx.send(Cmd::Stop(tx));
        if let Ok(recording) = rx.recv_timeout(STOP_WAIT) {
            return Stopped { recording, stuck: false };
        }
        // The microphone's callback only holds the lock briefly, but it's
        // the audio system's thread: don't wait on it for long either.
        let recording = match self.shared.try_lock_for(Duration::from_millis(500)) {
            Some(mut s) => {
                s.capturing = false;
                Recording { samples: std::mem::take(&mut s.buf), rate: s.rate, peak: s.peak }
            }
            None => Recording::default(),
        };
        (self.levels)(0.0);
        Stopped { recording, stuck: true }
    }

    /// While recording: what was recorded from sample `from` on (at the
    /// microphone's rate), and that rate. A long dictation is transcribed
    /// piece by piece this way while it's being made.
    pub fn peek(&self, from: usize) -> (Vec<f32>, u32) {
        let s = self.shared.lock();
        let new = if s.capturing { s.buf.get(from..).map(<[f32]>::to_vec).unwrap_or_default() } else { Vec::new() };
        (new, s.rate)
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(Cmd::Cancel);
    }

    /// Opens the microphone to report its level without recording (the
    /// setup assistant's microphone test), or stops that.
    pub fn meter(&self, device: Option<String>, on: bool) {
        let _ = self.tx.send(Cmd::Meter { device, on });
    }

    /// Open (or close) the microphone ahead of time.
    pub fn set_warm(&self, device: Option<String>, warm: bool) {
        let _ = self.tx.send(Cmd::Warm { device, warm });
    }

    pub fn cue(&self, cue: Cue, volume: f32) {
        let _ = self.tx.send(Cmd::Cue(cue, volume));
    }
}

struct AudioThread {
    levels: LevelSink,
    problems: ProblemSink,
    shared: Arc<Mutex<Shared>>,
    stream: Option<(cpal::Stream, Option<String>)>,
    /// The open stream reported an error it won't recover from (the
    /// microphone was unplugged): it's reopened when next needed.
    dead: Arc<AtomicBool>,
    warm: bool,
    cue_stream: Option<(cpal::Stream, Instant)>,
}

impl AudioThread {
    fn new(levels: LevelSink, problems: ProblemSink) -> Self {
        AudioThread {
            levels,
            problems,
            dead: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(Mutex::new(Shared {
                capturing: false,
                metering: false,
                rate: SAMPLE_RATE,
                channels: 1,
                buf: Vec::new(),
                preroll: VecDeque::new(),
                preroll_cap: 0,
                peak: 0.0,
            })),
            stream: None,
            warm: false,
            cue_stream: None,
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(cmd) => cmd,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Close an idle cue stream so we don't hold the sink open.
                    if self
                        .cue_stream
                        .as_ref()
                        .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(3))
                    {
                        self.cue_stream = None;
                    }
                    continue;
                }
                Err(_) => return,
            };
            match cmd {
                Cmd::Start { device, warm } => {
                    self.warm = warm;
                    if let Err(e) = self.ensure_stream(device) {
                        log::error!("microphone: {e:#}");
                        self.shared.lock().metering = false;
                        (self.problems)(format!("{e:#}"));
                        continue;
                    }
                    let mut s = self.shared.lock();
                    s.metering = false;
                    s.buf.clear();
                    // Room for half a minute, so the microphone's callback
                    // rarely has to wait for the buffer to grow.
                    let room = s.rate as usize * 30;
                    s.buf.reserve(room);
                    // Pre-roll: audio from just before the hotkey press.
                    let pre: Vec<f32> = s.preroll.drain(..).collect();
                    s.buf.extend(pre);
                    s.peak = 0.0;
                    s.capturing = true;
                }
                Cmd::Meter { device, on: true } => {
                    if let Err(e) = self.ensure_stream(device) {
                        log::error!("microphone: {e:#}");
                        (self.problems)(format!("{e:#}"));
                        continue;
                    }
                    self.shared.lock().metering = true;
                }
                Cmd::Meter { on: false, .. } => {
                    let capturing = {
                        let mut s = self.shared.lock();
                        s.metering = false;
                        s.capturing
                    };
                    if !capturing {
                        (self.levels)(0.0);
                        if !self.warm {
                            self.stream = None;
                        }
                    }
                }
                Cmd::Stop(reply) => {
                    let rec = self.finish();
                    let _ = reply.send(rec);
                }
                Cmd::Cancel => {
                    self.finish();
                }
                Cmd::Warm { device, warm } => {
                    self.warm = warm;
                    if warm {
                        if let Err(e) = self.ensure_stream(device) {
                            log::warn!("could not pre-open microphone: {e:#}");
                        }
                    } else if !self.in_use() {
                        self.stream = None;
                    }
                }
                Cmd::Cue(cue, volume) => {
                    if let Err(e) = self.play_cue(cue, volume) {
                        log::debug!("sound cue failed: {e:#}");
                    }
                }
            }
        }
    }

    /// The microphone is recording or being metered.
    fn in_use(&self) -> bool {
        let s = self.shared.lock();
        s.capturing || s.metering
    }

    fn finish(&mut self) -> Recording {
        let (raw, rate, peak, metering) = {
            let mut s = self.shared.lock();
            s.capturing = false;
            (std::mem::take(&mut s.buf), s.rate, s.peak, s.metering)
        };
        if !metering {
            (self.levels)(0.0);
            if !self.warm {
                self.stream = None;
            }
        }
        Recording { samples: raw, rate, peak }
    }

    fn ensure_stream(&mut self, device: Option<String>) -> anyhow::Result<()> {
        if let Some((_, current)) = &self.stream
            && *current == device && !self.dead.load(Ordering::Relaxed)
        {
            return Ok(());
        }
        self.stream = None;
        self.dead.store(false, Ordering::Relaxed);
        let host = cpal::default_host();
        let dev = match &device {
            Some(id) => id
                .parse::<cpal::DeviceId>()
                .ok()
                .and_then(|id| host.device_by_id(&id))
                .or_else(|| {
                    log::warn!("input device {id} not found; using default");
                    host.default_input_device()
                }),
            None => host.default_input_device(),
        }
        .ok_or_else(|| anyhow!("no microphone found"))?;

        let (config, format) = pick_config(&dev)?;
        {
            let mut s = self.shared.lock();
            s.rate = config.sample_rate;
            s.channels = config.channels;
            s.preroll_cap = (PREROLL.as_secs_f64() * config.sample_rate as f64) as usize;
            s.preroll.clear();
        }
        let stream = match format {
            SampleFormat::F32 => self.build::<f32>(&dev, config),
            SampleFormat::I16 => self.build::<i16>(&dev, config),
            SampleFormat::I32 => self.build::<i32>(&dev, config),
            SampleFormat::U8 => self.build::<u8>(&dev, config),
            other => Err(anyhow!("unsupported sample format {other:?}")),
        }?;
        stream.play().context("starting microphone stream")?;
        log::info!(
            "microphone open: {} @ {} Hz x{}",
            dev.description().map(|d| d.name().to_string()).unwrap_or_default(),
            config.sample_rate,
            config.channels
        );
        self.stream = Some((stream, device));
        Ok(())
    }

    fn build<T>(&self, dev: &cpal::Device, config: StreamConfig) -> anyhow::Result<cpal::Stream>
    where
        T: SizedSample + Send + 'static,
        f32: FromSample<T>,
    {
        let shared = self.shared.clone();
        let levels = self.levels.clone();
        let dead = self.dead.clone();
        let channels = config.channels as usize;
        // Level meter: RMS over ~16 ms blocks with fast attack / slow release.
        let block = (config.sample_rate as usize / 60).max(64);
        let mut acc = 0.0f32;
        let mut n = 0usize;
        let mut smooth = 0.0f32;
        let mut loudest = LoudestChannel::new(channels);
        let stream = dev.build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                // An interface with many inputs has the microphone on one
                // of them: averaging them all would bury it.
                let only = (channels > 2).then(|| {
                    loudest.pick(data.chunks(channels).map(|frame| frame.iter().map(|x| x.to_sample::<f32>())))
                });
                let mut s = shared.lock();
                let (capturing, metering) = (s.capturing, s.metering);
                for frame in data.chunks(channels) {
                    let v = match only {
                        Some(c) => frame.get(c).map_or(0.0, |x| x.to_sample::<f32>()),
                        None => frame.iter().map(|x| x.to_sample::<f32>()).sum::<f32>() / channels as f32,
                    };
                    if capturing {
                        s.buf.push(v);
                        s.peak = s.peak.max(v.abs());
                    } else if s.preroll_cap > 0 {
                        if s.preroll.len() >= s.preroll_cap {
                            s.preroll.pop_front();
                        }
                        s.preroll.push_back(v);
                    }
                    if capturing || metering {
                        acc += v * v;
                        n += 1;
                        if n >= block {
                            let rms = (acc / n as f32).sqrt();
                            // Speech RMS is usually 0.01..0.2; map to 0..1 perceptually.
                            let level = ((rms * 9.0).sqrt()).min(1.0);
                            smooth = if level > smooth { level } else { smooth * 0.82 + level * 0.18 };
                            levels(smooth);
                            acc = 0.0;
                            n = 0;
                        }
                    }
                }
            },
            move |e| {
                log::error!("microphone stream error: {e}");
                // A rerouted stream or a late buffer carries on; after
                // anything else the stream may never deliver again.
                if !matches!(e.kind(), cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::Xrun) {
                    dead.store(true, Ordering::Relaxed);
                }
            },
            None,
        )?;
        Ok(stream)
    }

    fn play_cue(&mut self, cue: Cue, volume: f32) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let dev = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no output device"))?;
        let supported = dev.default_output_config()?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let tone = Arc::new(synth_cue(cue, rate, volume));
        let pos = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = StreamConfig {
            channels: channels as u16,
            sample_rate: rate,
            buffer_size: small_buffer(supported.buffer_size(), rate, 10),
        };
        if supported.sample_format() != SampleFormat::F32 {
            return Err(anyhow!("cue output needs f32, got {:?}", supported.sample_format()));
        }
        let stream = dev.build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], _| {
                for frame in out.chunks_mut(channels) {
                    let i = pos.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let v = tone.get(i).copied().unwrap_or(0.0);
                    frame.fill(v);
                }
            },
            |e| log::debug!("cue stream error: {e}"),
            None,
        )?;
        stream.play()?;
        self.cue_stream = Some((stream, Instant::now()));
        Ok(())
    }
}

/// Which input of a many-channel interface carries the microphone: the one
/// with the most signal lately. It changes only when another is clearly
/// louder, so two inputs at a similar level don't trade places.
struct LoudestChannel {
    energy: Vec<f32>,
    current: usize,
}

impl LoudestChannel {
    fn new(channels: usize) -> Self {
        LoudestChannel { energy: vec![0.0; channels], current: 0 }
    }

    /// Takes the frames of one callback and returns the channel to use.
    fn pick<F: Iterator<Item = f32>>(&mut self, frames: impl Iterator<Item = F>) -> usize {
        // Earlier callbacks count for less.
        for e in &mut self.energy {
            *e *= 0.9;
        }
        for frame in frames {
            for (e, v) in self.energy.iter_mut().zip(frame) {
                *e += v * v;
            }
        }
        let (best, most) = self
            .energy
            .iter()
            .copied()
            .enumerate()
            .fold((self.current, 0.0f32), |best, (i, e)| if e > best.1 { (i, e) } else { best });
        if most > self.energy.get(self.current).copied().unwrap_or(0.0) * 2.0 {
            self.current = best;
        }
        self.current
    }
}

/// A small fixed buffer when the device allows it. PulseAudio otherwise picks
/// ~2 s record fragments, which would delay the start of every dictation.
///
/// macOS: never. There the buffer size belongs to the device, for all its
/// streams, and CoreAudio's own default is small already. The microphone and
/// the sound cues asking for different sizes on one device (a USB interface
/// used for both) made it reconfigure while recording, and the cue's stream
/// setup could deadlock with the microphone's callback.
fn small_buffer(range: &cpal::SupportedBufferSize, rate: u32, ms: u32) -> cpal::BufferSize {
    let frames = (rate * ms / 1000).max(64);
    match range {
        cpal::SupportedBufferSize::Range { min, max }
            if !cfg!(target_os = "macos") && (*min..=*max).contains(&frames) =>
        {
            cpal::BufferSize::Fixed(frames)
        }
        _ => cpal::BufferSize::Default,
    }
}

fn pick_config(dev: &cpal::Device) -> anyhow::Result<(StreamConfig, SampleFormat)> {
    // Linux: prefer the sound server doing the conversion to 16 kHz mono for
    // us. Elsewhere a non-native config reconfigures the device itself, for
    // every app (and switches Bluetooth headsets into their low-quality call
    // mode), so record in the device's own format and resample here.
    #[cfg(target_os = "linux")]
    if let Ok(ranges) = dev.supported_input_configs() {
        let mut best: Option<(StreamConfig, SampleFormat, u8)> = None;
        for r in ranges {
            if !r.contains_rate(SAMPLE_RATE) {
                continue;
            }
            let fmt = r.sample_format();
            let score = match fmt {
                SampleFormat::F32 => 3,
                SampleFormat::I16 => 2,
                SampleFormat::I32 => 1,
                _ => continue,
            } + if r.channels() == 1 { 4 } else { 0 };
            if best.as_ref().is_none_or(|b| score > b.2) {
                best = Some((
                    StreamConfig {
                        channels: r.channels().clamp(1, 2),
                        sample_rate: SAMPLE_RATE,
                        buffer_size: small_buffer(r.buffer_size(), SAMPLE_RATE, 20),
                    },
                    fmt,
                    score,
                ));
            }
        }
        if let Some((cfg, fmt, _)) = best {
            return Ok((cfg, fmt));
        }
    }
    let def = dev.default_input_config().context("querying microphone config")?;
    let mut cfg = def.config();
    cfg.buffer_size = small_buffer(def.buffer_size(), cfg.sample_rate, 20);
    Ok((cfg, def.sample_format()))
}

/// Windowed-sinc resampler (Blackman-Harris), good enough for ASR input.
/// The filter's taps depend only on where an output sample falls between
/// two input samples, and with whole-number rates there are only so many
/// such places (one for 48 kHz to 16 kHz, 160 for 44.1 kHz), so they're
/// worked out once: a minute of audio takes milliseconds, not a second.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let g = gcd(from, to);
    // Output n sits at input position n * down / up.
    let (up, down) = ((to / g) as usize, (from / g) as usize);
    if up > 1024 {
        return resample_direct(input, from, to);
    }
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.95;
    let half = (16.0 / cutoff).ceil() as usize;
    let width = 2 * half;
    // taps[phase * width + j] weighs input (center + 1 - half + j).
    let mut taps = vec![0f32; up * width];
    for phase in 0..up {
        let frac = phase as f64 / up as f64;
        for j in 0..width {
            let x = frac - (j as f64 + 1.0 - half as f64);
            taps[phase * width + j] = filter_tap(x, cutoff, half) as f32;
        }
        let row = &mut taps[phase * width..(phase + 1) * width];
        let sum: f32 = row.iter().sum();
        if sum.abs() > 1e-9 {
            row.iter_mut().for_each(|t| *t /= sum);
        }
    }
    let out_len = (input.len() as f64 * ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let pos = n * down;
        let (center, phase) = (pos / up, pos % up);
        let row = &taps[phase * width..(phase + 1) * width];
        let first = center as isize + 1 - half as isize;
        if first >= 0 && first as usize + width <= input.len() {
            let window = &input[first as usize..first as usize + width];
            out.push(window.iter().zip(row).map(|(a, b)| a * b).sum());
        } else {
            // At the edges, only the taps that land on input, renormalized.
            let (mut acc, mut wsum) = (0f32, 0f32);
            for (j, t) in row.iter().enumerate() {
                let k = first + j as isize;
                if k >= 0 && (k as usize) < input.len() {
                    acc += input[k as usize] * t;
                    wsum += t;
                }
            }
            out.push(if wsum.abs() > 1e-9 { acc / wsum } else { 0.0 });
        }
    }
    out
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The filter's weight for an input sample `x` input samples away.
fn filter_tap(x: f64, cutoff: f64, half: usize) -> f64 {
    let sinc = if x.abs() < 1e-9 {
        1.0
    } else {
        let a = std::f64::consts::PI * x * cutoff;
        a.sin() / a
    };
    let w_pos = (x / half as f64 + 1.0) / 2.0;
    let w = if (0.0..=1.0).contains(&w_pos) {
        let p = 2.0 * std::f64::consts::PI * w_pos;
        0.35875 - 0.48829 * p.cos() + 0.14128 * (2.0 * p).cos() - 0.01168 * (3.0 * p).cos()
    } else {
        0.0
    };
    sinc * w
}

/// The same filter computed tap by tap: for rates without a small common
/// ratio (and to check the fast path against).
fn resample_direct(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.95;
    let half_taps = (16.0 / cutoff).ceil() as isize;
    let out_len = (input.len() as f64 * ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    let n_in = input.len() as isize;
    for n in 0..out_len {
        let t = n as f64 / ratio;
        let center = t.floor() as isize;
        let mut acc = 0.0f64;
        let mut wsum = 0.0f64;
        for k in (center - half_taps + 1)..=(center + half_taps) {
            if k < 0 || k >= n_in {
                continue;
            }
            let c = filter_tap(t - k as f64, cutoff, half_taps as usize);
            acc += input[k as usize] as f64 * c;
            wsum += c;
        }
        out.push(if wsum.abs() > 1e-9 { (acc / wsum) as f32 } else { 0.0 });
    }
    out
}

/// Short, soft sine "blips". Rising = start, falling = stop.
fn synth_cue(cue: Cue, rate: u32, volume: f32) -> Vec<f32> {
    let notes: &[(f32, f32)] = match cue {
        Cue::Start => &[(587.33, 0.055), (880.0, 0.085)],
        Cue::Lock => &[(880.0, 0.05), (880.0, 0.0), (1174.66, 0.08)],
        Cue::Stop => &[(880.0, 0.055), (587.33, 0.085)],
        Cue::Cancel => &[(392.0, 0.06), (293.66, 0.09)],
    };
    let mut out = Vec::new();
    for &(freq, secs) in notes {
        let n = (secs * rate as f32) as usize;
        if freq == 0.0 || n == 0 {
            out.extend(std::iter::repeat_n(0.0, (0.02 * rate as f32) as usize));
            continue;
        }
        for i in 0..n {
            let t = i as f32 / rate as f32;
            // Quick attack, exponential decay; a touch of 2nd harmonic for warmth.
            let env = (t / 0.006).min(1.0) * (-t * 28.0).exp();
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.85
                + (4.0 * std::f32::consts::PI * freq * t).sin() * 0.15;
            out.push(s * env * volume * 0.5);
        }
    }
    // Tail so the stream doesn't cut the last note's release.
    out.extend(std::iter::repeat_n(0.0, (0.05 * rate as f32) as usize));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_input_with_the_microphone() {
        let mut loudest = LoudestChannel::new(4);
        // Noise on every input, speech on the third.
        let frames = |speech: f32| {
            (0..160).map(move |i| {
                let noise = 0.001 * ((i % 7) as f32 - 3.0);
                [noise, -noise, speech * (i as f32 * 0.3).sin(), noise].into_iter()
            })
        };
        assert_eq!(loudest.pick(frames(0.0)), 0);
        assert_eq!(loudest.pick(frames(0.2)), 2);
        // It stays there through a pause.
        assert_eq!(loudest.pick(frames(0.0)), 2);
    }

    #[test]
    fn resample_preserves_tone() {
        let from = 48_000;
        let input: Vec<f32> = (0..from)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / from as f32).sin())
            .collect();
        let out = resample(&input, from, SAMPLE_RATE);
        assert_eq!(out.len(), SAMPLE_RATE as usize);
        // Compare against an ideal 440 Hz sine at 16 kHz (skip edges).
        let err: f32 = out[1000..15000]
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let t = (i + 1000) as f32 / SAMPLE_RATE as f32;
                (v - (2.0 * std::f32::consts::PI * 440.0 * t).sin()).abs()
            })
            .fold(0.0, f32::max);
        assert!(err < 0.02, "max error {err}");
    }

    #[test]
    fn fast_resampling_matches_the_filter() {
        for from in [48_000, 44_100, 22_050, 8_000] {
            let input: Vec<f32> = (0..from / 2).map(|i| ((i as f32) * 0.37).sin() * 0.3 + ((i as f32) * 0.05).cos() * 0.2).collect();
            let fast = resample(&input, from, SAMPLE_RATE);
            let direct = resample_direct(&input, from, SAMPLE_RATE);
            assert_eq!(fast.len(), direct.len(), "{from}");
            let err = fast.iter().zip(&direct).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(err < 1e-4, "{from}: {err}");
        }
    }
}
