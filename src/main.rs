//! lancam - serve your webcam as an NDI stream on the local network.

mod audio;
mod capture;
mod engine;
mod ndi;
mod tray;
mod web;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};

use crate::capture::{list_devices, pick_mode, resolve_device};
use crate::engine::{AudioSpec, StreamConfig, Streamer};

#[derive(Parser)]
#[command(name = "lancam", version, about = "Serve your webcam and any audio as an NDI stream on the local network")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// List capture devices and modes, then exit
    #[arg(short, long)]
    list: bool,

    /// Detach into the background (log and pid file under the XDG state dir)
    #[arg(short = 'B', long)]
    background: bool,

    /// No periodic status line
    #[arg(short, long)]
    quiet: bool,

    #[command(flatten)]
    stream: StreamArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a systray controller for a running `lancam ui` service
    Tray {
        /// Port of the service to control
        #[arg(long, default_value_t = 8765)]
        port: u16,
    },

    /// Run the web control panel instead of streaming directly
    Ui {
        /// Bind address
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port
        #[arg(long, default_value_t = 8765)]
        port: u16,

        /// Detach into the background (log and pid file under the XDG state dir)
        #[arg(short = 'B', long)]
        background: bool,
    },
}

#[derive(Args)]
struct StreamArgs {
    /// Camera: /dev/videoN, index N, or card-name substring (default: first capture device)
    #[arg(short, long)]
    device: Option<String>,

    /// NDI source name (default: camera card name)
    #[arg(short, long)]
    name: Option<String>,

    /// NDI group names, comma separated
    #[arg(short, long, default_value = "")]
    groups: String,

    /// Requested width
    #[arg(long, default_value_t = 1920)]
    width: u32,

    /// Requested height
    #[arg(long, default_value_t = 1080)]
    height: u32,

    /// Requested fps
    #[arg(long, default_value_t = 60.0)]
    fps: f32,

    /// Capture fourcc: MJPG (default), YUYV, NV12, ...
    #[arg(long = "format", default_value = "MJPG")]
    fourcc: String,

    /// Also stream audio from the camera's microphone (if it has one)
    #[arg(short, long)]
    audio: bool,

    /// Stream audio from the ALSA card whose name contains SUBSTR; implies --audio
    #[arg(long)]
    audio_card: Option<String>,

    /// Audio sample rate
    #[arg(long, default_value_t = 48000)]
    audio_rate: u32,

    /// Audio channels
    #[arg(long, default_value_t = 2)]
    audio_channels: u32,

    /// Never stream audio, even with --audio/--audio-card
    #[arg(long)]
    video_only: bool,

    /// Hidden: send synthetic audio only, for debugging the NDI audio path
    #[arg(long, hide = true)]
    selftest_audio: bool,
}

fn selftest_audio() -> i32 {
    let sender = match ndi::Sender::new("lancam audio selftest", "") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return 1;
        }
    };
    eprintln!(
        "INFO: selftest source: {}",
        ndi::advertised_name("lancam audio selftest")
    );
    let (rate, ch, frames) = (48000usize, 2usize, 480usize);
    let mut planar = vec![0f32; ch * frames];
    let mut t = 0f32;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(6) {
        for i in 0..frames {
            let v = (t * 440.0 * 2.0 * std::f32::consts::PI / rate as f32).sin() * 0.2;
            planar[i] = v;
            planar[frames + i] = v;
            t += 1.0;
        }
        sender.send_audio(&planar, ch as i32, frames as i32, rate as i32, (frames * 4) as i32);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    eprintln!("INFO: selftest done");
    0
}

fn print_devices() -> i32 {
    let devices = list_devices(true);
    if devices.is_empty() {
        println!("No video capture devices found.");
        return 1;
    }
    println!("Capture devices:");
    for dev in &devices {
        println!("  {:<13} {}  ({})", dev.path, dev.card, dev.bus);
        let mut grouped: Vec<(String, u32, u32, Vec<f32>)> = Vec::new();
        for m in &dev.modes {
            match grouped
                .iter_mut()
                .find(|g| g.0 == m.fourcc && g.1 == m.width && g.2 == m.height)
            {
                Some(g) => {
                    if !g.3.contains(&m.fps) {
                        g.3.push(m.fps);
                    }
                }
                None => grouped.push((m.fourcc.clone(), m.width, m.height, vec![m.fps])),
            }
        }
        grouped.sort_by_key(|g| std::cmp::Reverse(g.1 * g.2));
        for (fcc, w, h, mut fps_list) in grouped {
            fps_list.sort_by(|a, b| b.partial_cmp(a).unwrap());
            let fps_str = fps_list
                .iter()
                .map(|f| format!("{f}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("      {fcc}  {w}x{h} @ {fps_str}");
        }
    }
    let cards = audio::list_cards();
    if !cards.is_empty() {
        println!("Audio capture cards:");
        for c in &cards {
            println!("  {}  {}", c.device, c.label);
        }
    }
    0
}

fn run_headless(args: &StreamArgs, quiet: bool) -> i32 {
    let dev = match resolve_device(args.device.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return 1;
        }
    };
    let mode = pick_mode(&dev, &args.fourcc, args.width, args.height, args.fps);
    let audio: Option<AudioSpec> = if (args.audio || args.audio_card.is_some()) && !args.video_only
    {
        let needle = args.audio_card.as_deref().unwrap_or(&dev.card);
        match audio::resolve_card(needle) {
            Ok(device) => Some(AudioSpec {
                device,
                rate: args.audio_rate,
                channels: args.audio_channels,
            }),
            Err(e) => {
                eprintln!("ERROR: {e}");
                return 1;
            }
        }
    } else {
        None
    };

    let streamer = Streamer::new(StreamConfig {
        device_path: dev.path.clone(),
        card: dev.card.clone(),
        mode,
        ndi_name: args.name.clone().unwrap_or_default(),
        groups: args.groups.clone(),
        audio,
    });

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let _ = ctrlc::set_handler(move || stop_flag.store(true, Ordering::SeqCst));

    if let Err(e) = streamer.start(false) {
        eprintln!("ERROR: {e}");
        return 1;
    }
    eprintln!(
        "INFO: serving {} as NDI source - Ctrl-C to stop",
        ndi::advertised_name(args.name.as_deref().unwrap_or(&dev.card))
    );

    let mut last_line = Instant::now();
    while !stop.load(Ordering::SeqCst) && streamer.state() != engine::State::Stopped {
        std::thread::sleep(Duration::from_millis(200));
        if !quiet && last_line.elapsed() >= Duration::from_secs(2) {
            last_line = Instant::now();
            let s = streamer.stats();
            let tally = match s.tally {
                Some((p, v)) => match (p, v) {
                    (true, true) => "PGM/PRV".to_string(),
                    (true, false) => "PGM".to_string(),
                    (false, true) => "PRV".to_string(),
                    (false, false) => "--".to_string(),
                },
                None => "--".to_string(),
            };
            eprint!(
                "\r\x1b[2K\u{25cf} {} {} | {:5.1} fps | viewers {} | tally {:<7} | {}",
                s.resolution, s.fourcc, s.fps, s.viewers, tally, s.source
            );
        }
    }
    if !quiet {
        eprintln!();
    }
    streamer.stop();
    if let Some(err) = streamer.error() {
        eprintln!("ERROR: {err}");
        return 1;
    }
    println!("Stopped.");
    0
}

/// State directory for the pid/log files: $XDG_RUNTIME_DIR (or
/// /run/user/<uid>), falling back to ~/.local/state/lancam.
fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    let mut dir = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    if !dir.is_dir() {
        dir = std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .join(".local/state/lancam");
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Fork into the background: the child becomes a session leader with its
/// output appended to lancam.log; the parent writes the pid file and exits.
fn daemonize() -> anyhow::Result<()> {
    let dir = state_dir()?;
    let log_path = dir.join("lancam.log");
    let pid_path = dir.join("lancam.pid");
    use std::os::fd::AsRawFd;
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork failed");
        }
        if pid == 0 {
            libc::setsid();
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            libc::dup2(log.as_raw_fd(), 1);
            libc::dup2(log.as_raw_fd(), 2);
            let devnull = std::fs::File::open("/dev/null")?;
            libc::dup2(devnull.as_raw_fd(), 0);
            std::mem::forget(log);
            std::mem::forget(devnull);
            std::fs::write(&pid_path, std::process::id().to_string())?;
            return Ok(());
        }
        println!(
            "lancam running in background (pid {pid}); log: {}; stop: kill $(cat {})",
            log_path.display(),
            pid_path.display()
        );
        std::process::exit(0);
    }
}

fn main() -> std::process::ExitCode {
    // Die quietly on closed pipes (e.g. `lancam --list | head`) like any
    // other Unix tool, instead of panicking on EPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    match &cli.cmd {
        Some(Cmd::Tray { port }) => match tray::run(*port) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ERROR: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Some(Cmd::Ui {
            host,
            port,
            background,
        }) => {
            if *background {
                if let Err(e) = daemonize() {
                    eprintln!("ERROR: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
            match web::serve(host, *port) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        None => {
            if cli.background {
                if let Err(e) = daemonize() {
                    eprintln!("ERROR: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
            if cli.list {
                return std::process::ExitCode::from(print_devices() as u8);
            }
            if cli.stream.selftest_audio {
                return std::process::ExitCode::from(selftest_audio() as u8);
            }
            std::process::ExitCode::from(run_headless(&cli.stream, cli.quiet) as u8)
        }
    }
}
