//! Local web control panel: configure, preview, go live, watch stats.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::audio;
use crate::capture::{self, DeviceInfo, Mode};
use crate::engine::{AudioSpec, State as StreamState, StreamConfig, Streamer};

const HTML: &str = include_str!("webui.html");

/// Last successfully started configuration, remembered across restarts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedSettings {
    pub device: String,
    pub card: String,
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub groups: String,
    #[serde(default)]
    pub audio: String,
    #[serde(default = "default_rate")]
    pub audio_rate: u32,
    #[serde(default = "default_channels")]
    pub audio_channels: u32,
}

fn settings_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".config")
        });
    base.join("lancam").join("settings.json")
}

fn load_saved() -> Option<SavedSettings> {
    let text = std::fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_settings(s: &SavedSettings) {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Validate saved settings against what is present right now: unknown
/// camera falls back to the first device, stale modes get re-negotiated,
/// vanished audio targets fall back to video-only.
fn resolve_saved(devices: &[DeviceInfo]) -> serde_json::Value {
    let fallback_device = devices.first().map(|d| d.path.clone()).unwrap_or_default();
    let Some(saved) = load_saved() else {
        return serde_json::json!({ "device": fallback_device });
    };
    let matched = devices
        .iter()
        .find(|d| d.path == saved.device)
        .or_else(|| devices.iter().find(|d| d.card == saved.card));
    let dev = matched.clone().or_else(|| devices.first());
    let Some(dev) = dev else {
        return serde_json::json!({ "device": fallback_device });
    };
    // Camera gone: its saved mode is meaningless, so start from the defaults.
    let mode = match matched {
        Some(_) => capture::pick_mode(dev, &saved.fourcc, saved.width, saved.height, saved.fps),
        None => capture::pick_mode(dev, "MJPG", 1920, 1080, 60.0),
    };
    let cards = audio::list_cards();
    let audio = match saved.audio.as_str() {
        "" | "auto" => saved.audio.clone(),
        other if cards.iter().any(|c| c.device == other) => other.to_string(),
        _ => String::new(), // saved mic/virtual source is gone
    };
    serde_json::json!({
        "device": dev.path,
        "fourcc": mode.fourcc,
        "width": mode.width,
        "height": mode.height,
        "fps": mode.fps,
        "name": saved.name,
        "groups": saved.groups,
        "audio": audio,
        "audio_rate": saved.audio_rate,
        "audio_channels": saved.audio_channels,
    })
}

struct WebState {
    streamer: Streamer,
    devices: Mutex<(Instant, Vec<DeviceInfo>)>,
}

type App = Arc<WebState>;

fn cached_devices(state: &App, refresh: bool) -> Vec<DeviceInfo> {
    let mut guard = state.devices.lock().unwrap();
    if refresh || guard.1.is_empty() || guard.0.elapsed() > Duration::from_secs(30) {
        *guard = (Instant::now(), capture::list_devices(true));
    }
    guard.1.clone()
}

async fn index() -> Html<&'static str> {
    Html(HTML)
}

async fn api_state(State(state): State<App>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "state": state.streamer.state(),
        "error": state.streamer.error(),
        "stats": state.streamer.stats(),
    }))
}

async fn api_devices(State(state): State<App>) -> Json<Vec<DeviceInfo>> {
    Json(cached_devices(&state, false))
}

async fn api_audio() -> Json<Vec<audio::AudioCard>> {
    Json(audio::list_cards())
}

async fn api_settings(State(state): State<App>) -> Json<serde_json::Value> {
    let devices = cached_devices(&state, false);
    Json(resolve_saved(&devices))
}

async fn api_levels(State(state): State<App>) -> Json<serde_json::Value> {
    let (levels, active) = state.streamer.audio_levels();
    Json(serde_json::json!({ "levels": levels, "active": active }))
}

#[derive(Deserialize)]
struct StartReq {
    preview: bool,
    device: String,
    fourcc: String,
    width: u32,
    height: u32,
    fps: f32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    groups: String,
    /// null | "auto" | an ALSA plughw device
    #[serde(default)]
    audio: Option<String>,
    #[serde(default = "default_rate")]
    audio_rate: u32,
    #[serde(default = "default_channels")]
    audio_channels: u32,
}

fn default_rate() -> u32 {
    48000
}
fn default_channels() -> u32 {
    2
}

async fn api_start(State(state): State<App>, Json(req): Json<StartReq>) -> Response {
    if state.streamer.state() != StreamState::Stopped {
        return (StatusCode::CONFLICT, "stream already running").into_response();
    }
    let dev = match cached_devices(&state, false)
        .into_iter()
        .find(|d| d.path == req.device)
    {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "unknown device").into_response(),
    };
    let mode: Mode = capture::pick_mode(&dev, &req.fourcc, req.width, req.height, req.fps);
    let audio = match req.audio.as_deref() {
        None => None,
        Some("auto") => match crate::engine::auto_mic(&dev.card) {
            Some(spec) => Some(AudioSpec {
                rate: req.audio_rate,
                channels: req.audio_channels,
                ..spec
            }),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "{} has no microphone - pick 'No audio (video only)' or another card",
                        dev.card
                    ),
                )
                    .into_response()
            }
        },
        Some(device) => Some(AudioSpec {
            device: device.to_string(),
            rate: req.audio_rate,
            channels: req.audio_channels,
        }),
    };
    let config = StreamConfig {
        device_path: dev.path.clone(),
        card: dev.card.clone(),
        mode,
        ndi_name: req.name.clone(),
        groups: req.groups.clone(),
        audio,
    };
    let chosen = config.mode.clone();
    save_settings(&SavedSettings {
        device: dev.path.clone(),
        card: dev.card.clone(),
        fourcc: chosen.fourcc.clone(),
        width: chosen.width,
        height: chosen.height,
        fps: chosen.fps,
        name: req.name.clone(),
        groups: req.groups.clone(),
        audio: req.audio.clone().unwrap_or_default(),
        audio_rate: req.audio_rate,
        audio_channels: req.audio_channels,
    });
    state.streamer.set_config(config);
    match state.streamer.start(req.preview) {
        Ok(()) => Json(serde_json::json!({"ok": true, "mode": chosen})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_stop(State(state): State<App>) -> Json<serde_json::Value> {
    state.streamer.stop();
    Json(serde_json::json!({"ok": true}))
}

const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
const PREVIEW_MAX_WIDTH: u32 = 960;

async fn preview(State(state): State<App>) -> Response {
    let streamer = state.streamer.clone();
    let stream = IntervalStream::new(tokio::time::interval(PREVIEW_INTERVAL)).filter_map(
        move |_| {
            let frame = streamer.frame()?;
            let (mut w, mut h) = (frame.width, frame.height);
            let mut src = &frame.bgra[..];
            let scaled: Vec<u8>;
            let factor = w.div_ceil(PREVIEW_MAX_WIDTH);
            if factor > 1 {
                scaled = downscale_bgra(&frame.bgra, w, h, factor);
                w /= factor;
                h /= factor;
                src = &scaled;
            }
            let rgb = bgra_to_rgb(src);
            let mut jpg: Vec<u8> = Vec::new();
            let enc = jpeg_encoder::Encoder::new(&mut jpg, 80);
            enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                .ok()?;
            let chunk = format!(
                "--lancam\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpg.len()
            );
            let mut out = chunk.into_bytes();
            out.extend_from_slice(&jpg);
            out.extend_from_slice(b"\r\n");
            Some(Ok::<Bytes, Infallible>(Bytes::from(out)))
        },
    );
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=lancam",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn downscale_bgra(src: &[u8], w: u32, h: u32, f: u32) -> Vec<u8> {
    let (w, h, f) = (w as usize, h as usize, f as usize);
    let nw = w / f;
    let nh = h / f;
    let mut out = Vec::with_capacity(nw * nh * 4);
    for row in 0..nh {
        for col in 0..nw {
            let mut acc = [0u32; 3];
            for dy in 0..f {
                let base = ((row * f + dy) * w + col * f) * 4;
                for dx in 0..f {
                    let p = base + dx * 4;
                    acc[0] += src[p] as u32;
                    acc[1] += src[p + 1] as u32;
                    acc[2] += src[p + 2] as u32;
                }
            }
            let n = (f * f) as u32;
            out.extend_from_slice(&[
                (acc[0] / n) as u8,
                (acc[1] / n) as u8,
                (acc[2] / n) as u8,
                255,
            ]);
        }
    }
    out
}

fn bgra_to_rgb(src: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(src.len() / 4 * 3);
    for px in src.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    rgb
}

pub fn serve(host: &str, port: u16) -> anyhow::Result<()> {
    let state: App = Arc::new(WebState {
        streamer: Streamer::new(StreamConfig {
            device_path: String::new(),
            card: String::new(),
            mode: Mode {
                fourcc: "MJPG".into(),
                width: 1920,
                height: 1080,
                fps: 60.0,
            },
            ndi_name: String::new(),
            groups: String::new(),
            audio: None,
        }),
        devices: Mutex::new((Instant::now(), Vec::new())),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/devices", get(api_devices))
        .route("/api/audio", get(api_audio))
        .route("/api/levels", get(api_levels))
        .route("/api/settings", get(api_settings))
        .route("/api/start", post(api_start))
        .route("/api/stop", post(api_stop))
        .route("/preview", get(preview))
        .with_state(state.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((host, port)).await?;
        eprintln!("INFO: control panel on http://{host}:{port}");
        let server = axum::serve(listener, app);
        tokio::select! {
            r = server => r.map_err(anyhow::Error::from),
            _ = tokio::signal::ctrl_c() => {
                eprintln!("INFO: shutting down");
                state.streamer.stop();
                Ok(())
            }
        }
    })
}
