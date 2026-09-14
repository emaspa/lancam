//! V4L2 capture: device enumeration, mode negotiation, frame decode.
//!
//! MJPG frames are decoded with zune-jpeg; YUYV/NV12 are converted by hand.
//! Everything lands in a BGRA buffer, which is what we hand to NDI.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use v4l::buffer::Type;
use v4l::capability::Flags as CapsFlags;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::capture::Parameters;
use v4l::video::traits::Capture;
use v4l::{Format, FourCC};

// VIDIOC_ENUM_FRAMESIZES / _FRAMEINTERVALS: the v4l crate does not wrap
// these two, and their kernel structs are stable ABI.
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = 0xC02C_564A;
const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong = 0xC034_564B;
const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;

#[repr(C)]
#[derive(Default)]
struct FrmsizeEnum {
    index: u32,
    pixel_format: u32,
    typ: u32,
    // union: discrete {width, height} + stepwise padding
    width: u32,
    height: u32,
    pad: [u32; 4],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Default)]
struct FrmivalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    typ: u32,
    // union: discrete {numerator, denominator} + stepwise padding
    numerator: u32,
    denominator: u32,
    pad: [u32; 4],
    reserved: [u32; 2],
}

fn enum_frame_sizes(fd: i32, pixel_format: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let mut e = FrmsizeEnum {
            index,
            pixel_format,
            ..Default::default()
        };
        if unsafe { libc::ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut e) } < 0 {
            break;
        }
        if e.typ == V4L2_FRMSIZE_TYPE_DISCRETE {
            out.push((e.width, e.height));
        }
        index += 1;
    }
    out
}

fn enum_frame_fps(fd: i32, pixel_format: u32, width: u32, height: u32) -> Vec<f32> {
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let mut e = FrmivalEnum {
            index,
            pixel_format,
            width,
            height,
            ..Default::default()
        };
        if unsafe { libc::ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut e) } < 0 {
            break;
        }
        if e.typ == V4L2_FRMSIZE_TYPE_DISCRETE && e.numerator > 0 {
            let fps = (e.denominator as f32 / e.numerator as f32 * 10.0).round() / 10.0;
            if !out.contains(&fps) {
                out.push(fps);
            }
        }
        index += 1;
    }
    out
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Mode {
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub path: String,
    pub card: String,
    pub bus: String,
    pub modes: Vec<Mode>,
}

pub fn list_devices(with_modes: bool) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/video4linux") else {
        return out;
    };
    let mut nodes: Vec<(u32, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let idx = n.strip_prefix("video")?.parse().ok()?;
            let card = fs::read_to_string(e.path().join("name"))
                .unwrap_or_default()
                .split(':')
                .next()
                .unwrap_or("?")
                .trim()
                .to_string();
            Some((idx, card))
        })
        .collect();
    nodes.sort_by_key(|(idx, _)| *idx);

    for (idx, card) in nodes {
        let path = format!("/dev/video{idx}");
        if !Path::new(&path).exists() {
            continue;
        }
        let Ok(dev) = Device::with_path(&path) else {
            continue;
        };
        let Ok(caps) = dev.query_caps() else {
            continue;
        };
        if !caps.capabilities.contains(CapsFlags::VIDEO_CAPTURE) {
            continue;
        }
        let modes = if with_modes {
            enum_modes(&dev).unwrap_or_default()
        } else {
            Vec::new()
        };
        out.push(DeviceInfo {
            path,
            card,
            bus: caps.bus,
            modes,
        });
    }
    out
}

fn enum_modes(dev: &Device) -> Result<Vec<Mode>> {
    let fd = dev.handle().fd();
    let mut modes = Vec::new();
    for desc in dev.enum_formats()? {
        let fourcc = desc.fourcc.to_string();
        if !decodable(&fourcc) {
            continue;
        }
        let repr = u32::from_le_bytes(desc.fourcc.repr);
        for (width, height) in enum_frame_sizes(fd, repr) {
            for fps in enum_frame_fps(fd, repr, width, height) {
                modes.push(Mode {
                    fourcc: fourcc.clone(),
                    width,
                    height,
                    fps,
                });
            }
        }
    }
    Ok(modes)
}

pub fn resolve_device(spec: Option<&str>) -> Result<DeviceInfo> {
    let devices = list_devices(false);
    if devices.is_empty() {
        bail!("no video capture devices found (check /dev/video*)");
    }
    let Some(spec) = spec else {
        return Ok(devices.into_iter().next().unwrap());
    };
    if let Some(path) = spec.strip_prefix("/dev/") {
        return devices
            .into_iter()
            .find(|d| d.path.ends_with(path))
            .with_context(|| format!("{spec} is not a capture device"));
    }
    if let Ok(idx) = spec.parse::<u32>() {
        return devices
            .into_iter()
            .find(|d| d.path.ends_with(&format!("video{idx}")))
            .with_context(|| format!("no capture device with index {idx}"));
    }
    let needle = spec.to_lowercase();
    devices
        .into_iter()
        .find(|d| d.card.to_lowercase().contains(&needle))
        .with_context(|| format!("no capture device matching {spec:?}"))
}

/// Formats lancam can decode in-process. NDI takes uncompressed frames, so
/// compressed camera formats need a decoder; H264 has none here.
pub const DECODABLE: &[&str] = &["MJPG", "JPEG", "YUYV", "NV12"];

pub fn decodable(fourcc: &str) -> bool {
    DECODABLE.contains(&fourcc)
}

/// Honor the requested mode when supported; otherwise pick the closest
/// decodable one.
pub fn pick_mode(dev: &DeviceInfo, fourcc: &str, width: u32, height: u32, fps: f32) -> Mode {
    let supported = decodable(fourcc)
        && (dev.modes.is_empty()
            || dev.modes.iter().any(|m| {
                m.fourcc == fourcc
                    && m.width == width
                    && m.height == height
                    && (m.fps - fps).abs() < 0.11
            }));
    if supported {
        return Mode {
            fourcc: fourcc.to_string(),
            width,
            height,
            fps,
        };
    }
    if !decodable(fourcc) {
        eprintln!("WARN: {fourcc} needs an H.264-class decoder lancam does not have; picking a decodable mode");
    }
    let same_fourcc: Vec<&Mode> = dev
        .modes
        .iter()
        .filter(|m| m.fourcc == fourcc && decodable(&m.fourcc))
        .collect();
    let fallback: Vec<&Mode> = dev
        .modes
        .iter()
        .filter(|m| decodable(&m.fourcc))
        .collect();
    let candidates: &[&Mode] = if same_fourcc.is_empty() { &fallback } else { &same_fourcc };
    let want_area = (width * height) as i64;
    match candidates
        .iter()
        .min_by_key(|m| {
            (
                m.fourcc != "MJPG",
                ((m.width * m.height) as i64 - want_area).abs(),
                ((m.fps - fps).abs() * 10.0) as i64,
            )
        })
        .cloned()
        .cloned()
    {
        Some(m) => {
            eprintln!(
                "WARN: {} does not support {fourcc} {width}x{height}@{fps}; using {} {}x{}@{}",
                dev.card, m.fourcc, m.width, m.height, m.fps
            );
            m
        }
        None => Mode {
            fourcc: if decodable(fourcc) { fourcc.to_string() } else { "MJPG".into() },
            width,
            height,
            fps,
        },
    }
}

pub struct Camera {
    // The reader thread owns the device and the mmap stream: dequeuing and
    // requeueuing happens there, so buffers are held for a memcpy of the
    // compressed frame only. Some UVC firmware (Logi 4K observed) drops
    // captures that start while no buffer is idle, which a decode-in-capture-
    // thread design triggers at high frame rates.
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    worker: Option<thread::JoinHandle<()>>,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub fourcc: String,
}

#[derive(Debug)]
pub enum ReadOutcome {
    Frame,
    Timeout,
}

impl Camera {
    pub fn open(path: &str, width: u32, height: u32, fps: f32, fourcc: &str) -> Result<Camera> {
        let dev = Device::with_path(path).with_context(|| format!("cannot open {path}"))?;
        let cc: &[u8; 4] = fourcc.as_bytes().try_into().unwrap_or(b"MJPG");
        let fmt = Format::new(width, height, FourCC::new(cc));
        dev.set_format(&fmt)?;
        let parm = Parameters::with_fps(fps.round().max(1.0) as u32);
        let _ = dev.set_params(&parm);
        let actual = dev.format()?;

        let (tx, rx) = mpsc::sync_channel(4);
        let worker = thread::Builder::new()
            .name("lancam-capture".into())
            .spawn(move || {
                let Ok(mut stream) = Stream::with_buffers(&dev, Type::VideoCapture, 4) else {
                    return;
                };
                stream.set_timeout(Duration::from_secs(2));
                // next() queues all buffers and starts the stream on first
                // call; afterwards it requeues the previous buffer, so the
                // mmap buffer is only held for this copy.
                loop {
                    match stream.next() {
                        Ok((buf, _meta)) => {
                            if tx.send(buf.to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("spawn capture thread")?;

        Ok(Camera {
            rx: Some(rx),
            worker: Some(worker),
            width: actual.width,
            height: actual.height,
            fps,
            fourcc: actual.fourcc.to_string(),
        })
    }

    /// Grab one frame and convert it to BGRA in `dst`.
    pub fn read_bgra(&mut self, dst: &mut Vec<u8>) -> Result<ReadOutcome> {
        let Some(rx) = self.rx.as_ref() else {
            return Ok(ReadOutcome::Timeout);
        };
        let Ok(comp) = rx.recv_timeout(Duration::from_millis(200)) else {
            return Ok(ReadOutcome::Timeout);
        };
        match self.fourcc.as_str() {
            "MJPG" | "JPEG" => decode_mjpg(&comp, dst),
            "YUYV" => yuyv_to_bgra(&comp, dst, self.width, self.height),
            "NV12" => nv12_to_bgra(&comp, dst, self.width, self.height),
            other => bail!("unsupported capture format {other}"),
        }?;
        Ok(ReadOutcome::Frame)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // Drop the receiver first so the reader thread's send fails; joining
        // then runs streamoff + munmap + close inside it.
        self.rx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn decode_mjpg(jpg: &[u8], dst: &mut Vec<u8>) -> Result<()> {
    let mut decoder = zune_jpeg::JpegDecoder::new(jpg);
    let rgb = decoder.decode().context("mjpeg decode failed")?;
    dst.clear();
    dst.resize(rgb.len() / 3 * 4, 0);
    for (px, out) in rgb.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
        out[0] = px[2];
        out[1] = px[1];
        out[2] = px[0];
        out[3] = 255;
    }
    Ok(())
}

#[inline]
fn yuv_pixel(y: i32, u: i32, v: i32, dst: &mut Vec<u8>) {
    let c = y - 16;
    let b = (298 * c + 516 * v + 128) >> 8;
    let g = (298 * c - 100 * u - 208 * v + 128) >> 8;
    let r = (298 * c + 409 * u + 128) >> 8;
    dst.extend_from_slice(&[
        b.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        r.clamp(0, 255) as u8,
        255,
    ]);
}

fn yuyv_to_bgra(src: &[u8], dst: &mut Vec<u8>, width: u32, height: u32) -> Result<()> {
    if src.len() < (width * height * 2) as usize {
        bail!("short YUYV frame");
    }
    dst.clear();
    dst.reserve((width * height * 4) as usize);
    for mp in src.chunks_exact(4) {
        let (u, v) = (mp[1] as i32 - 128, mp[3] as i32 - 128);
        yuv_pixel(mp[0] as i32, u, v, dst);
        yuv_pixel(mp[2] as i32, u, v, dst);
    }
    Ok(())
}

fn nv12_to_bgra(src: &[u8], dst: &mut Vec<u8>, width: u32, height: u32) -> Result<()> {
    let (w, h) = (width as usize, height as usize);
    if src.len() < w * h * 3 / 2 {
        bail!("short NV12 frame");
    }
    let (y_plane, uv_plane) = src.split_at(w * h);
    dst.clear();
    dst.reserve(w * h * 4);
    for row in 0..h {
        for col in 0..w {
            let uv = (row / 2) * w + (col & !1);
            yuv_pixel(
                y_plane[row * w + col] as i32,
                uv_plane[uv] as i32 - 128,
                uv_plane[uv + 1] as i32 - 128,
                dst,
            );
        }
    }
    Ok(())
}
