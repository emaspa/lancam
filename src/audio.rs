//! Microphone capture straight from ALSA: S16LE in, float32 blocks out.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};

pub const BLOCK_FRAMES: usize = 480; // 10 ms at 48 kHz

#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioCard {
    pub device: String, // "pw:<node-name>" or plughw:C,D
    pub label: String,
    pub alt: String, // secondary match string (PipeWire node name)
}

/// PipeWire capture targets: real sources plus sink monitors (which is how
/// virtual devices such as mix buses expose their audio).
pub fn list_pw_nodes() -> Vec<AudioCard> {
    let Ok(out) = std::process::Command::new("pw-dump").output() else {
        return Vec::new();
    };
    let Ok(objs) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for o in objs {
        let props = o.get("info").and_then(|i| i.get("props")).unwrap_or(&serde_json::Value::Null);
        let mc = props.get("media.class").and_then(|v| v.as_str()).unwrap_or("");
        let name = props.get("node.name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() || !(mc == "Audio/Source" || mc == "Audio/Sink") {
            continue;
        }
        let desc = props
            .get("node.description")
            .and_then(|v| v.as_str())
            .unwrap_or(name);
        let label = if mc == "Audio/Sink" {
            format!("{desc} (monitor)")
        } else {
            desc.to_string()
        };
        if cards.iter().any(|c: &AudioCard| c.device == format!("pw:{name}")) {
            continue;
        }
        cards.push(AudioCard {
            device: format!("pw:{name}"),
            label,
            alt: name.to_string(),
        });
    }
    cards
}

pub fn list_cards() -> Vec<AudioCard> {
    // PipeWire bridges ALSA hardware too, so when it is running its node list
    // is the complete one (and the only place virtual devices exist).
    let pw = list_pw_nodes();
    if !pw.is_empty() {
        return pw;
    }
    let mut cards = Vec::new();
    let Ok(text) = fs::read_to_string("/proc/asound/cards") else {
        return cards;
    };
    for line in text.lines() {
        // Format: " 0 [ShortName     ]: Long card name"
        let t = line.trim_start();
        let (Some(sp), Ok(idx)) = (t.find(' '), t.split(' ').next().unwrap_or("").parse::<u32>())
        else {
            continue;
        };
        let rest = &t[sp..];
        let (Some(open),) = (rest.find('['),) else { continue };
        let (Some(close),) = (rest[open..].find(']'),) else { continue };
        let Some(label) = rest[open + close + 1..]
            .strip_prefix(':')
            .map(|s| s.trim().to_string())
        else {
            continue;
        };
        let Ok(pcm) = fs::read_dir(format!("/proc/asound/card{idx}")) else {
            continue;
        };
        let mut devices: Vec<u32> = pcm
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let dev = n.strip_prefix("pcm")?.strip_suffix('c')?.parse().ok()?;
                Some(dev)
            })
            .collect();
        devices.sort_unstable();
        if let Some(dev) = devices.first() {
            cards.push(AudioCard {
                device: format!("plughw:{idx},{dev}"),
                label,
                alt: String::new(),
            });
        }
    }
    cards
}

pub fn resolve_card(substring: &str) -> Result<String> {
    let needle = substring.to_lowercase().replace('_', " ");
    for card in list_cards() {
        let hay = format!(
            "{} {}",
            card.label,
            card.alt.replace('_', " ")
        )
        .to_lowercase();
        if hay.contains(&needle) {
            return Ok(card.device);
        }
    }
    bail!("no ALSA capture card matching {substring:?}");
}

pub struct Mic {
    pub rx: mpsc::Receiver<Vec<f32>>, // interleaved (frames * channels) f32
    pub alive: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    // pw-cat child, kept so teardown can kill a pump blocked in read().
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl Mic {
    pub fn spawn(device: &str, rate: u32, channels: u32) -> Result<Mic> {
        let device = device.to_string();
        let (tx, rx): (SyncSender<Vec<f32>>, _) = mpsc::sync_channel(50);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_flag = alive.clone();
        let child: Arc<std::sync::Mutex<Option<std::process::Child>>> =
            Arc::new(std::sync::Mutex::new(None));
        let child_flag = child.clone();
        let handle = thread::Builder::new()
            .name("lancam-audio".into())
            .spawn(move || {
                let res = if let Some(target) = device.strip_prefix("pw:") {
                    pump_pw(target, rate, channels, &tx, &alive_flag, &child_flag)
                } else {
                    pump(&device, rate, channels, &tx, &alive_flag)
                };
                if let Err(e) = res {
                    eprintln!("WARN: audio capture ended: {e}");
                }
                alive_flag.store(false, Ordering::SeqCst);
            })
            .context("spawn audio thread")?;
        Ok(Mic {
            rx,
            alive,
            handle: Some(handle),
            child,
        })
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        // Kill pw-cat first: the pump may be blocked reading its stdout.
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn pump_pw(
    target: &str,
    rate: u32,
    channels: u32,
    tx: &SyncSender<Vec<f32>>,
    alive: &AtomicBool,
    child_slot: &Arc<std::sync::Mutex<Option<std::process::Child>>>,
) -> Result<()> {
    let child = std::process::Command::new("pw-cat")
        .args([
            "-r",
            "--target",
            target,
            "--rate",
            &rate.to_string(),
            "--channels",
            &channels.to_string(),
            "--format",
            "s16",
            "-",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start pw-cat for {target}"))?;
    *child_slot.lock().unwrap() = Some(child);
    let mut stdout = {
        let mut guard = child_slot.lock().unwrap();
        guard
            .as_mut()
            .and_then(|c| c.stdout.take())
            .context("pw-cat stdout")?
    };
    pump_reader(&mut stdout, channels, tx, alive)
}

fn pump(
    device: &str,
    rate: u32,
    channels: u32,
    tx: &SyncSender<Vec<f32>>,
    alive: &AtomicBool,
) -> Result<()> {
    use alsa::pcm::{Access, Format as AlsaFormat, HwParams};
    use alsa::{Direction, ValueOr, PCM};
    // Non-blocking so the thread can notice `alive` flipping even when the
    // device goes silent; otherwise teardown would wait on a stuck read.
    let pcm = PCM::new(device, Direction::Capture, true)
        .with_context(|| format!("cannot open ALSA device {device}"))?;
    {
        let hw = HwParams::any(&pcm)?;
        hw.set_access(Access::RWInterleaved)?;
        hw.set_format(AlsaFormat::S16LE)?;
        hw.set_rate(rate, ValueOr::Nearest)?;
        hw.set_channels(channels)?;
        pcm.hw_params(&hw)?;
    }
    let io = pcm.io_i16()?;
    let mut buf = vec![0i16; BLOCK_FRAMES * channels as usize];
    // Non-blocking reads return whatever is ready (often 1 ms slivers); NDI
    // wants sane blocks, so accumulate to BLOCK_FRAMES before sending.
    let mut acc: Vec<f32> = Vec::with_capacity(BLOCK_FRAMES * channels as usize * 2);
    let mut consecutive_errors = 0u32;
    while alive.load(Ordering::SeqCst) {
        match io.readi(&mut buf) {
            Ok(0) => {
                thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Err(e) => {
                // A stray EAGAIN right after start is normal; a long error
                // streak means the device is gone or misconfigured.
                consecutive_errors += 1;
                if consecutive_errors == 200 {
                    eprintln!("WARN: alsa readi failing repeatedly: {e}");
                }
                if consecutive_errors > 400 {
                    bail!("audio device {device} stopped delivering: {e}");
                }
                thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Ok(frames) => {
                consecutive_errors = 0;
                acc.extend(buf[..frames * channels as usize].iter().map(|s| *s as f32 / 32768.0));
            }
        }
        let want = BLOCK_FRAMES * channels as usize;
        while acc.len() >= want {
            let block: Vec<f32> = acc.drain(..want).collect();
            // Full channel means the consumer fell behind; drop rather than
            // grow latency.
            let _ = tx.try_send(block);
        }
    }
    Ok(())
}

/// Shared accumulator: reads little-endian i16 PCM and emits BLOCK_FRAMES
/// float32 blocks, dropping rather than growing latency when behind.
fn pump_reader(
    src: &mut dyn std::io::Read,
    channels: u32,
    tx: &SyncSender<Vec<f32>>,
    alive: &AtomicBool,
) -> Result<()> {
    let want_samples = BLOCK_FRAMES * channels as usize;
    let mut raw = vec![0u8; want_samples * 2];
    let mut acc: Vec<f32> = Vec::with_capacity(want_samples * 2);
    let mut consecutive_errors = 0u32;
    while alive.load(Ordering::SeqCst) {
        match read_exact_or_eof(src, &mut raw) {
            Ok(true) => {
                consecutive_errors = 0;
                acc.extend(
                    raw.chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
                );
            }
            Ok(false) => break, // EOF: producer gone
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 20 {
                    eprintln!("WARN: audio read failing repeatedly: {e}");
                }
                if consecutive_errors > 100 {
                    bail!("audio source stopped delivering: {e}");
                }
                thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        }
        while acc.len() >= want_samples {
            let block: Vec<f32> = acc.drain(..want_samples).collect();
            let _ = tx.try_send(block);
        }
    }
    Ok(())
}

fn read_exact_or_eof(src: &mut dyn std::io::Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false), // EOF: producer gone
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
