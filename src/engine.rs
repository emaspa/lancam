//! The capture -> NDI pipeline, owned by one thread, shared with CLI and web.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use arc_swap::ArcSwapOption;
use serde::Serialize;

use crate::audio::{resolve_card, Mic};
use crate::capture::{Camera, Mode, ReadOutcome};
use crate::ndi::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Stopped,
    Preview,
    Live,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioSpec {
    pub device: String,
    pub rate: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamConfig {
    pub device_path: String,
    pub card: String,
    pub mode: Mode,
    pub ndi_name: String,
    pub groups: String,
    pub audio: Option<AudioSpec>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub fps: f32,
    pub viewers: u32,
    pub tally: Option<(bool, bool)>,
    pub resolution: String,
    pub fourcc: String,
    pub camera_fps: f32,
    pub audio: bool,
    pub source: String,
    pub uptime: f32,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct Inner {
    state: Mutex<State>,
    error: Mutex<Option<String>>,
    stats: Mutex<Stats>,
    frame: ArcSwapOption<Frame>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    config: Mutex<StreamConfig>,
    // Per-channel audio peaks, 0..1, decayed per block; plus when the last
    // block arrived so the UI can tell "silent" from "not capturing".
    levels: Mutex<Vec<f32>>,
    last_audio: Mutex<Instant>,
}

#[derive(Clone)]
pub struct Streamer {
    inner: Arc<Inner>,
}

impl Streamer {
    pub fn new(config: StreamConfig) -> Self {
        Streamer {
            inner: Arc::new(Inner {
                state: Mutex::new(State::Stopped),
                error: Mutex::new(None),
                stats: Mutex::new(Stats::default()),
                frame: ArcSwapOption::empty(),
                stop: AtomicBool::new(false),
                thread: Mutex::new(None),
                config: Mutex::new(config),
                levels: Mutex::new(Vec::new()),
                last_audio: Mutex::new(Instant::now()),
            }),
        }
    }

    pub fn state(&self) -> State {
        *self.inner.state.lock().unwrap()
    }

    pub fn error(&self) -> Option<String> {
        self.inner.error.lock().unwrap().clone()
    }

    pub fn stats(&self) -> Stats {
        self.inner.stats.lock().unwrap().clone()
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.inner.frame.load_full()
    }

    /// Current per-channel audio levels (0..1) and whether blocks are
    /// actually arriving (vs. configured but silent/dead).
    pub fn audio_levels(&self) -> (Vec<f32>, bool) {
        let levels = self.inner.levels.lock().unwrap().clone();
        let active = self.inner.last_audio.lock().unwrap().elapsed()
            < Duration::from_millis(500);
        (levels, active)
    }

    pub fn set_config(&self, config: StreamConfig) {
        *self.inner.config.lock().unwrap() = config;
    }

    pub fn start(&self, preview_only: bool) -> Result<()> {
        if self.state() != State::Stopped {
            anyhow::bail!("already {:?}", self.state());
        }
        *self.inner.error.lock().unwrap() = None;
        *self.inner.stats.lock().unwrap() = Stats::default();
        self.inner.stop.store(false, Ordering::SeqCst);
        *self.inner.state.lock().unwrap() = if preview_only {
            State::Preview
        } else {
            State::Live
        };
        let inner = self.inner.clone();
        let handle = thread::Builder::new()
            .name("lancam-stream".into())
            .spawn(move || run(inner, preview_only))?;
        *self.inner.thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        let handle = self.inner.thread.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn run(inner: Arc<Inner>, preview_only: bool) {
    if let Err(e) = run_inner(&inner, preview_only) {
        eprintln!("ERROR: stream failed: {e}");
        *inner.error.lock().unwrap() = Some(e.to_string());
    }
    *inner.state.lock().unwrap() = State::Stopped;
    inner.frame.store(None);
}

fn run_inner(inner: &Arc<Inner>, preview_only: bool) -> Result<()> {
    let config = inner.config.lock().unwrap().clone();
    let mode = config.mode.clone();
    let mut cam = Camera::open(
        &config.device_path,
        mode.width,
        mode.height,
        mode.fps,
        &mode.fourcc,
    )?;
    eprintln!(
        "INFO: capturing {}: {}x{} {} @ {} fps",
        config.device_path, cam.width, cam.height, cam.fourcc, cam.fps
    );

    let (fps_n, fps_d) = rational_fps(cam.fps);
    let chosen_name = if config.ndi_name.is_empty() {
        config.card.clone()
    } else {
        config.ndi_name.clone()
    };
    let sender: Option<Sender> = if preview_only {
        None
    } else {
        Some(Sender::new(&chosen_name, &config.groups)?)
    };
    let advertised = if sender.is_some() {
        crate::ndi::advertised_name(&chosen_name)
    } else {
        String::new()
    };
    if sender.is_some() {
        eprintln!("INFO: NDI source created: {advertised}");
    }

    // Audio is captured in preview as well, so the meters can prove the
    // source works before anything goes live; preview just never sends it.
    let mut mic: Option<Mic> = match &config.audio {
        Some(spec) => match Mic::spawn(&spec.device, spec.rate, spec.channels) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("WARN: audio capture failed: {e}; video-only");
                None
            }
        },
        None => None,
    };
    if let Some(spec) = &config.audio {
        *inner.levels.lock().unwrap() = vec![0.0; spec.channels as usize];
    }

    let mut stats = Stats {
        resolution: format!("{}x{}", cam.width, cam.height),
        fourcc: cam.fourcc.clone(),
        camera_fps: cam.fps,
        audio: mic.is_some(),
        source: advertised,
        ..Default::default()
    };
    *inner.stats.lock().unwrap() = stats.clone();

    // Two-slot buffer pool: publish the just-sent frame for the preview
    // endpoint and reuse the previously published allocation next round.
    let mut buf: Vec<u8> = Vec::with_capacity((cam.width * cam.height * 4) as usize);
    let mut reuse: Option<Arc<Frame>> = None;
    let mut frames: u64 = 0;
    let mut last = Instant::now();
    let started = last;
    let mut planar: Vec<f32> = Vec::new();

    while !inner.stop.load(Ordering::SeqCst) {
        if mic.as_ref().is_some_and(|m| !m.alive.load(Ordering::SeqCst)) {
            eprintln!("WARN: audio stream died; continuing video-only");
            mic = None;
            stats.audio = false;
        }
        if let Some(mic) = &mic {
            let channels = config.audio.as_ref().map(|a| a.channels).unwrap_or(2) as usize;
            let rate = config.audio.as_ref().map(|a| a.rate).unwrap_or(48000) as i32;
            while let Ok(block) = mic.rx.try_recv() {
                let frames_n = block.len() / channels;
                // Per-channel peaks for the meters, with a quick decay so
                // the bars fall back between transients.
                {
                    let mut levels = inner.levels.lock().unwrap();
                    for (i, s) in block.iter().enumerate() {
                        let ch = i % channels;
                        let a = s.abs();
                        if levels.get(ch).map(|v| a > *v).unwrap_or(false) {
                            levels[ch] = a;
                        }
                    }
                    for v in levels.iter_mut() {
                        *v *= 0.9;
                    }
                }
                *inner.last_audio.lock().unwrap() = Instant::now();
                if let Some(sender) = &sender {
                    planar.clear();
                    planar.resize(channels * frames_n, 0.0);
                    for (i, s) in block.iter().enumerate() {
                        planar[(i % channels) * frames_n + i / channels] = *s;
                    }
                    sender.send_audio(&planar, channels as i32, frames_n as i32, rate, (frames_n * 4) as i32);
                }
            }
        }

        match cam.read_bgra(&mut buf) {
            Ok(ReadOutcome::Frame) => {}
            Ok(ReadOutcome::Timeout) => continue,
            Err(e) => {
                eprintln!("WARN: frame read failed: {e}; reopening camera");
                drop(cam);
                let mut delay = Duration::from_millis(500);
                loop {
                    if inner.stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    thread::sleep(delay);
                    match Camera::open(
                        &config.device_path,
                        mode.width,
                        mode.height,
                        mode.fps,
                        &mode.fourcc,
                    ) {
                        Ok(c) => {
                            cam = c;
                            break;
                        }
                        Err(_) => delay = (delay * 2).min(Duration::from_secs(10)),
                    }
                }
                continue;
            }
        }

        if let Some(sender) = &sender {
            sender.send_video(cam.width, cam.height, &buf, fps_n, fps_d);
        }
        frames += 1;

        let mut pool_buf = reuse
            .take()
            .and_then(|f| Arc::try_unwrap(f).ok())
            .map(|f| f.bgra)
            .unwrap_or_default();
        std::mem::swap(&mut pool_buf, &mut buf);
        let frame = Arc::new(Frame {
            width: cam.width,
            height: cam.height,
            bgra: pool_buf,
        });
        reuse = inner.frame.swap(Some(frame));

        let now = Instant::now();
        if now.duration_since(last) >= Duration::from_secs(1) {
            stats.fps = (frames as f32 / now.duration_since(last).as_secs_f32() * 10.0).round()
                / 10.0;
            stats.viewers = sender.as_ref().map(|s| s.connections()).unwrap_or(0);
            stats.tally = sender
                .as_ref()
                .and_then(|s| s.tally())
                .map(|t| (t.on_program, t.on_preview));
            stats.uptime = now.duration_since(started).as_secs_f32();
            *inner.stats.lock().unwrap() = stats.clone();
            frames = 0;
            last = now;
        }
    }
    Ok(())
}

/// NDI wants a rational frame rate: 59.94 -> (30000, 500), 60 -> (60, 1).
pub fn rational_fps(fps: f32) -> (i32, i32) {
    let scaled = (fps * 1000.0).round() as i64;
    let g = gcd(scaled, 1000);
    ((scaled / g) as i32, (1000 / g) as i32)
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Resolve "the camera's own microphone", if the card has one.
pub fn auto_mic(card: &str) -> Option<AudioSpec> {
    resolve_card(card).ok().map(|device| AudioSpec {
        device,
        rate: 48000,
        channels: 2,
    })
}
