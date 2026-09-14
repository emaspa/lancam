//! Systray controller: a StatusNotifierItem that mirrors a running
//! `lancam ui` service and can start/stop the last used settings.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SvcState {
    Offline,
    #[default]
    Stopped,
    Preview,
    Live,
}

#[derive(Default)]
struct Snapshot {
    state: SvcState,
    fps: f32,
    viewers: u32,
    audio: bool,
}

#[derive(Default, Deserialize)]
struct StateReply {
    state: String,
    stats: StatsReply,
}

#[derive(Default, Deserialize)]
struct StatsReply {
    fps: f32,
    viewers: u32,
    audio: bool,
}

#[derive(Default, Deserialize)]
struct SettingsReply {
    device: String,
    #[serde(default = "def_fourcc")]
    fourcc: String,
    #[serde(default = "def_width")]
    width: u32,
    #[serde(default = "def_height")]
    height: u32,
    #[serde(default = "def_fps")]
    fps: f32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    groups: String,
    #[serde(default)]
    audio: String,
    #[serde(default = "def_rate")]
    audio_rate: u32,
    #[serde(default = "def_channels")]
    audio_channels: u32,
}

fn def_fourcc() -> String {
    "MJPG".into()
}
fn def_width() -> u32 {
    1920
}
fn def_height() -> u32 {
    1080
}
fn def_fps() -> f32 {
    60.0
}
fn def_rate() -> u32 {
    48000
}
fn def_channels() -> u32 {
    2
}

fn get_json<T: serde::de::DeserializeOwned>(base: &str, path: &str) -> Option<T> {
    let resp = ureq::get(&format!("{base}{path}")).call().ok()?;
    resp.into_json().ok()
}

fn start_last(base: &str) -> Result<(), String> {
    let s: SettingsReply = get_json(base, "/api/settings").ok_or("cannot read settings")?;
    let audio = if s.audio.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(s.audio.clone())
    };
    let body = serde_json::json!({
        "preview": false,
        "device": s.device,
        "fourcc": s.fourcc,
        "width": s.width,
        "height": s.height,
        "fps": s.fps,
        "name": s.name,
        "groups": s.groups,
        "audio": audio,
        "audio_rate": s.audio_rate,
        "audio_channels": s.audio_channels,
    });
    ureq::post(&format!("{base}/api/start"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn stop(base: &str) -> Result<(), String> {
    ureq::post(&format!("{base}/api/stop"))
        .call()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn in_rounded(x: i32, y: i32, x0: i32, y0: i32, w: i32, h: i32, r: i32) -> bool {
    if x < x0 || x > x0 + w - 1 || y < y0 || y > y0 + h - 1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x0 + w - 1 - r);
    let cy = y.clamp(y0 + r, y0 + h - 1 - r);
    (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
}

/// 24x24 monochrome webcam glyph in the panel foreground colour, Breeze
/// style: outlined body and stand, empty lens ring, and a centre dot that
/// appears only when live. Offline dims the whole glyph.
fn state_icon(state: SvcState) -> ksni::Icon {
    let alpha: u8 = match state {
        SvcState::Offline => 90,
        _ => 255,
    };
    const N: i32 = 24;
    let mut data = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let body = in_rounded(x, y, 3, 4, 18, 13, 3)
                && !in_rounded(x, y, 5, 6, 14, 9, 2);
            let stand = (x == 11 || x == 12) && (y == 17 || y == 18)
                || (y == 19 || y == 20) && (8..=15).contains(&x);
            let dx = x as f32 - 11.5;
            let dy = y as f32 - 10.0;
            let d2 = dx * dx + dy * dy;
            // Lens ring stays empty; the centre dot means live.
            let lens = (d2 <= 9.0 && d2 >= 2.6) || (state == SvcState::Live && d2 <= 1.6);
            if body || stand || lens {
                data.extend_from_slice(&[alpha, 255, 255, 255]);
            } else {
                data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    ksni::Icon {
        width: N,
        height: N,
        data,
    }
}

pub struct Tray {
    base: String,
    snap: Arc<Mutex<Snapshot>>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "lancam".into()
    }

    fn title(&self) -> String {
        "lancam".into()
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let state = self.snap.lock().unwrap().state;
        vec![state_icon(state)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let s = self.snap.lock().unwrap();
        let description = match s.state {
            SvcState::Offline => "service offline".to_string(),
            SvcState::Stopped => "stopped".to_string(),
            SvcState::Preview => format!("previewing, {:.0} fps", s.fps),
            SvcState::Live => format!(
                "live, {:.0} fps, {} viewers, audio {}",
                s.fps,
                s.viewers,
                if s.audio { "on" } else { "off" }
            ),
        };
        ksni::ToolTip {
            title: "lancam".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let s = self.snap.lock().unwrap();
        let (state, fps, viewers, audio) = (s.state, s.fps, s.viewers, s.audio);
        drop(s);
        let label = match state {
            SvcState::Offline => "service offline".to_string(),
            SvcState::Stopped => "stopped".to_string(),
            SvcState::Preview => format!("previewing, {fps:.0} fps"),
            SvcState::Live => format!(
                "live, {fps:.0} fps, {viewers} viewers, audio {}",
                if audio { "on" } else { "off" }
            ),
        };
        let running = state == SvcState::Preview || state == SvcState::Live;
        // Menu callbacks must not block the tray, so HTTP calls hop to a
        // short-lived thread.
        vec![
            StandardItem {
                label,
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Go live (last settings)".into(),
                enabled: state == SvcState::Stopped,
                activate: Box::new(|this: &mut Self| {
                    let base = this.base.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = start_last(&base) {
                            eprintln!("lancam tray: start failed: {e}");
                        }
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop".into(),
                enabled: running,
                activate: Box::new(|this: &mut Self| {
                    let base = this.base.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = stop(&base) {
                            eprintln!("lancam tray: stop failed: {e}");
                        }
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open control panel".into(),
                enabled: state != SvcState::Offline,
                activate: Box::new(|this: &mut Self| {
                    let url = this.base.clone();
                    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit tray".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn run(port: u16) -> anyhow::Result<()> {
    use ksni::blocking::TrayMethods;
    let base = format!("http://127.0.0.1:{port}");
    let snap = Arc::new(Mutex::new(Snapshot::default()));
    let handle = Tray {
        base: base.clone(),
        snap: snap.clone(),
    }
    .spawn()?;

    let poll_handle = handle.clone();
    let poll_base = base.clone();
    let poll_snap = snap;
    std::thread::Builder::new()
        .name("lancam-tray-poll".into())
        .spawn(move || loop {
            let next = match get_json::<StateReply>(&poll_base, "/api/state") {
                Some(r) => Snapshot {
                    state: match r.state.as_str() {
                        "live" => SvcState::Live,
                        "preview" => SvcState::Preview,
                        _ => SvcState::Stopped,
                    },
                    fps: r.stats.fps,
                    viewers: r.stats.viewers,
                    audio: r.stats.audio,
                },
                None => Snapshot {
                    state: SvcState::Offline,
                    ..Default::default()
                },
            };
            *poll_snap.lock().unwrap() = next;
            // Nudge the tray into emitting property-changed signals so the
            // icon and tooltip refresh without opening the menu.
            poll_handle.update(|_| {});
            std::thread::sleep(Duration::from_secs(2));
        })?;

    eprintln!("INFO: tray registered; talking to {base}");
    loop {
        std::thread::park();
    }
}
