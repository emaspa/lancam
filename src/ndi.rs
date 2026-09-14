//! Minimal safe wrapper over the NDI SDK C ABI, loaded at runtime via dlopen.
//!
//! Only the send side is wrapped: that is all a source needs. Struct layouts
//! match `Processing.NDI.Lib.h` (SDK 6).

use std::ffi::{c_char, c_void, CString};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use libloading::{Library, Symbol};

pub const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");
const FRAME_FORMAT_PROGRESSIVE: i32 = 1;
/// "Make up a timecode at send time" — required on audio frames when the
/// stream is not SDK-clocked; a bare 0 gets the frames dropped.
const SEND_TIMECODE_SYNTHESIZE: i64 = i64::MAX;

#[repr(C)]
struct SendCreateT {
    ndi_name: *const c_char,
    groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

#[repr(C)]
struct VideoFrameV2T {
    xres: i32,
    yres: i32,
    fourcc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    picture_aspect_ratio: f32,
    frame_format_type: i32,
    timecode: i64,
    data: *const u8,
    line_stride_in_bytes: i32,
    metadata: *const c_char,
    timestamp: i64,
}

// Field order must match Processing.NDI.structs.h exactly: timecode and
// p_data come before channel_stride_in_bytes.
#[repr(C)]
struct AudioFrameV2T {
    sample_rate: i32,
    no_channels: i32,
    no_samples: i32,
    timecode: i64,
    data: *const f32,
    channel_stride_in_bytes: i32,
    metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
#[derive(Default)]
pub struct Tally {
    pub on_program: bool,
    pub on_preview: bool,
}

struct Fns {
    initialize: unsafe extern "C" fn() -> bool,
    send_create: unsafe extern "C" fn(*const SendCreateT) -> *mut c_void,
    send_destroy: unsafe extern "C" fn(*mut c_void),
    send_video: unsafe extern "C" fn(*mut c_void, *const VideoFrameV2T),
    send_audio: unsafe extern "C" fn(*mut c_void, *const AudioFrameV2T),
    send_get_no_connections: unsafe extern "C" fn(*mut c_void, u32) -> u32,
    send_get_tally: unsafe extern "C" fn(*mut c_void, *mut Tally, u32) -> bool,
}

static NDI: OnceLock<Result<(), String>> = OnceLock::new();
static NDI_FNS: OnceLock<Fns> = OnceLock::new();
static T0: OnceLock<Instant> = OnceLock::new();

fn load() -> Result<()> {
    let err = NDI.get_or_init(|| match load_inner() {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    });
    match err {
        Ok(()) => Ok(()),
        Err(msg) => bail!("{msg}"),
    }
}

fn load_inner() -> Result<()> {
    let candidates = ["libndi.so.6", "libndi.so", "/usr/lib/libndi.so.6"];
    let mut last_err = None;
    let lib = candidates
        .iter()
        .find_map(|name| match unsafe { Library::new(name) } {
            Ok(lib) => Some(lib),
            Err(e) => {
                last_err = Some(format!("{name}: {e}"));
                None
            }
        })
        .with_context(|| format!("cannot load NDI SDK ({})", last_err.unwrap_or_default()))?;

    unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T> {
        let s: Symbol<T> = unsafe { lib.get(name) }
            .with_context(|| format!("missing symbol {}", String::from_utf8_lossy(name)))?;
        Ok(*s)
    }
    let fns = unsafe {
        Fns {
            initialize: sym(&lib, b"NDIlib_initialize\0")?,
            send_create: sym(&lib, b"NDIlib_send_create\0")?,
            send_destroy: sym(&lib, b"NDIlib_send_destroy\0")?,
            send_video: sym(&lib, b"NDIlib_send_send_video_v2\0")?,
            send_audio: sym(&lib, b"NDIlib_send_send_audio_v2\0")?,
            send_get_no_connections: sym(&lib, b"NDIlib_send_get_no_connections\0")?,
            send_get_tally: sym(&lib, b"NDIlib_send_get_tally\0")?,
        }
    };
    if !unsafe { (fns.initialize)() } {
        bail!("NDIlib_initialize failed (unsupported CPU?)");
    }
    T0.get_or_init(Instant::now);
    let _ = NDI_FNS.set(fns);
    // The library is deliberately leaked: unloading libndi while send
    // handles exist is undefined behavior.
    std::mem::forget(lib);
    Ok(())
}

fn fns() -> &'static Fns {
    NDI_FNS.get().expect("ndi::load not called")
}

fn timestamp_100ns() -> i64 {
    // One monotonic epoch for every stream we send, so audio and video stay
    // on a single time base.
    T0.get_or_init(Instant::now).elapsed().as_nanos() as i64 / 100
}

pub struct Sender {
    handle: *mut c_void,
}

// The NDI SDK documents its send instance as thread-safe.
unsafe impl Send for Sender {}
unsafe impl Sync for Sender {}

impl Sender {
    pub fn new(name: &str, groups: &str) -> Result<Self> {
        load()?;
        let name = CString::new(name)?;
        let groups = CString::new(groups)?;
        let desc = SendCreateT {
            ndi_name: name.as_ptr(),
            groups: groups.as_ptr(),
            // We timestamp every frame ourselves on one monotonic clock; the
            // SDK's internal per-stream clocks differ and desync A/V.
            clock_video: false,
            clock_audio: false,
        };
        let handle = unsafe { (fns().send_create)(&desc) };
        if handle.is_null() {
            bail!("NDIlib_send_create failed");
        }
        Ok(Sender { handle })
    }

    pub fn send_video(
        &self,
        width: u32,
        height: u32,
        bgra: &[u8],
        fps_n: i32,
        fps_d: i32,
    ) {
        let frame = VideoFrameV2T {
            xres: width as i32,
            yres: height as i32,
            fourcc: FOURCC_BGRX,
            frame_rate_n: fps_n,
            frame_rate_d: fps_d,
            picture_aspect_ratio: width as f32 / height as f32,
            frame_format_type: FRAME_FORMAT_PROGRESSIVE,
            timecode: 0,
            data: bgra.as_ptr(),
            line_stride_in_bytes: (width * 4) as i32,
            metadata: std::ptr::null(),
            timestamp: timestamp_100ns(),
        };
        unsafe { (fns().send_video)(self.handle, &frame) };
    }

    /// `planar` is channel-major float32 audio, `channel_stride` bytes per plane.
    pub fn send_audio(&self, planar: &[f32], channels: i32, samples: i32, rate: i32, channel_stride: i32) {
        let frame = AudioFrameV2T {
            sample_rate: rate,
            no_channels: channels,
            no_samples: samples,
            timecode: SEND_TIMECODE_SYNTHESIZE,
            data: planar.as_ptr(),
            channel_stride_in_bytes: channel_stride,
            metadata: std::ptr::null(),
            timestamp: timestamp_100ns(),
        };
        unsafe { (fns().send_audio)(self.handle, &frame) };
    }

    pub fn connections(&self) -> u32 {
        unsafe { (fns().send_get_no_connections)(self.handle, 0) }
    }

    pub fn tally(&self) -> Option<Tally> {
        let mut t = Tally::default();
        unsafe { (fns().send_get_tally)(self.handle, &mut t, 0) }.then_some(t)
    }

}

/// NDI advertises sources as "MACHINE (name)"; mirror that for display.
pub fn advertised_name(name: &str) -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    if host.is_empty() {
        name.to_string()
    } else {
        format!("{host} ({name})")
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        unsafe { (fns().send_destroy)(self.handle) };
    }
}
