# lancam

Serve any V4L2 webcam as an NDI stream on your local network, with audio
from any source on the machine: the camera mic, an ALSA card, or a PipeWire
virtual device. A small web panel controls what goes out.

Point it at a camera and it advertises a full-bandwidth NDI source over
mDNS, ready for OBS, vMix, NDI Studio Monitor or any other NDI client on the
LAN. It ships as one binary. The NDI SDK loads at runtime via dlopen, V4L2
capture decodes MJPEG/YUYV/NV12 in-process, and the panel is an embedded
HTTP server.

## Build

```sh
cargo build --release
```

Runtime requirements:

- NDI SDK 6, installed separately as described below. lancam dlopens
  `libndi.so.6` and ships none of it.
- `avahi-daemon` running, for NDI discovery.
- `alsa-lib` for ALSA audio, and `pipewire` (`pw-dump`, `pw-cat`) for
  PipeWire audio targets.

## Installing the NDI SDK

The SDK is free but proprietary, and its license expects each user to install
it themselves, so lancam never bundles it.

Arch and derivatives:

```sh
yay -S ndi-sdk
```

This unpacks the official installer and places the library in `/usr/lib`,
where the dynamic loader finds it.

On other Linux distributions, download the SDK from
https://ndi.video/for-developers/ndi-sdk/ (accept the EULA), then:

```sh
tar xf Install_NDI_SDK_v6_Linux.tar.gz
sh Install_NDI_SDK_v6_Linux.sh
# the installer defaults to "$HOME/NDI SDK for Linux"; expose the library:
sudo install -m 755 "$HOME/NDI SDK for Linux/lib/x86_64-linux-gnu/libndi.so.6."* /usr/local/lib/
sudo ldconfig
```

Verify with `ldconfig -p | grep libndi`. If you keep the SDK in another
directory instead, list that directory in `/etc/ld.so.conf.d/` and run
`ldconfig`, or export `LD_LIBRARY_PATH` for the lancam process.

## Usage

```sh
# first capture device, 1080p60 MJPG, source named after the camera card
lancam

# pick a camera and mode
lancam -d 0 --width 3840 --height 2160 --fps 30
lancam -d /dev/video4 --format NV12

# audio: the camera's own mic, any ALSA card, or any PipeWire node
lancam -d 0 --audio
lancam --audio-card "Wave XLR"
lancam --audio-card OpenXLR_stream      # PipeWire virtual source or monitor

# explicitly video-only (also the default when no audio flag is given)
lancam --audio --video-only

# see what is available
lancam --list

# web control panel: pick camera/mode/audio, preview, go live, watch stats
lancam ui                 # http://127.0.0.1:8765
lancam ui --host 0.0.0.0  # expose on the LAN (the panel can start streams!)

# systray controller for a running service: state icon, go live, stop,
# open the panel. Needs a desktop with StatusNotifierItem support (KDE,
# or GNOME with the AppIndicator extension).
lancam tray

# detach into the background (works for `ui` and for headless streaming):
# pid file and log land in $XDG_RUNTIME_DIR (or ~/.local/state/lancam)
lancam ui --background
kill $(cat "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/lancam.pid")
```

Status line while running:

```
● 1920x1080 MJPG |  60.0 fps | viewers 1 | tally PRV | desk (camera card name)
```

### Flags

| Flag | Meaning |
|---|---|
| `-d, --device DEV` | `/dev/videoN`, index N, or card-name substring (default: first capture device) |
| `-n, --name NAME` | NDI source name (default: camera card name) |
| `-g, --groups G` | NDI group names, comma separated |
| `--width/--height/--fps/--format` | Requested mode (default 1920x1080@60 MJPG); falls back to the closest supported mode |
| `-a, --audio` | Stream audio from the camera's own mic, if it has one |
| `--audio-card SUBSTR` | Stream audio from the ALSA card or PipeWire node whose name contains SUBSTR (implies `--audio`) |
| `--video-only` | Never stream audio, overriding the audio flags |
| `-q / -v` | Quiet status line / verbose logging |
| `-B, --background` | Detach into the background; pid file + log under the XDG state dir |
| `ui --host --port` | Run the web control panel |

## Audio sources

When PipeWire is running, the audio picker in the UI and `--audio-card`
list its capture nodes. That includes hardware sources and sink monitors,
which is how virtual devices like mix buses and application captures expose
their audio. Without PipeWire you get ALSA capture cards instead. Audio and
video share one monotonic clock, so A/V stays in sync.

The panel shows per-channel audio meters whenever audio is configured.
Preview captures audio for the meters without sending it over NDI, so you
can check a source before going on air. Greyed-out meters mean the source
stopped delivering blocks.

Settings from the last successful start are remembered in
`~/.config/lancam/settings.json` and prefilled the next time the panel
loads. If the saved camera or audio target is gone, lancam falls back to
the first device and to video-only.

## Run as a user service

Two units ship in `packaging/`. `lancam.service` runs the control panel
headless. It can start at boot, captures nothing until a client asks, and
releases camera and microphone on Stop. `lancam-tray.service` adds the
systray icon. It needs a desktop session, so it joins your login rather
than boot.

```sh
cargo build --release
sudo install -m 755 target/release/lancam /usr/local/bin/
mkdir -p ~/.config/systemd/user
cp packaging/lancam.service packaging/lancam-tray.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now lancam
systemctl --user enable --now lancam-tray
loginctl enable-linger $USER    # start lancam at boot, before any login
```

The panel's REST endpoints (`/api/devices`, `/api/audio`, `/api/start`,
`/api/stop`, `/api/state`, `/api/levels`) are open to other controllers;
the tray is one client among possible others.

After rebuilding, refresh the installed binary and restart both units:

```sh
sudo install -m 755 target/release/lancam /usr/local/bin/
systemctl --user restart lancam lancam-tray
```

For a service that always captures, point `ExecStart` at plain `lancam`
with the flags you want.

## Notes

- **Full NDI, not NDI|HX.** 1080p60 runs at roughly 100-250 Mbit/s. Fine on
  gigabit LAN, not on Wi-Fi.
- **Firewall.** NDI needs mDNS (UDP 5353) plus TCP 5960 and the dynamic
  video ports. LAN clients usually work as-is. Check your firewall zone if
  not.
- **Decodable formats only.** The picker offers MJPG, YUYV and NV12. H264
  camera streams would need an H.264 decoder in front of NDI, so lancam
  skips them and falls back to the closest decodable mode.
- **One owner per camera.** The camera stays open for the life of the
  process. Stop lancam before using the webcam elsewhere.

## License

GPL-3.0-or-later. See `LICENSE`.

NDI® is a registered trademark of Vizrt NDI AB. This project is not
affiliated with or endorsed by Vizrt. It requires the separately licensed
NDI SDK (https://ndi.video) at runtime. The SDK is not redistributed here;
see "Installing the NDI SDK" above.
