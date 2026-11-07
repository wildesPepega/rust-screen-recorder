# rust-screen-recorder

ShadowPlay-style instant replay, recording and game capture for **KDE Plasma 6 (Wayland)**, written in Rust.

## Highlights

- **No portal dialog, ever.** Capture uses KWin's privileged `zkde_screencast_unstable_v1`
  protocol directly (the way Spectacle does it). The KDE "Select what to share" dialog is never shown,
  and no permission or restore token is written to any KDE / portal config.
- **Game mode.** The game's *window* is captured (not the screen), so overlays, notifications and
  alt-tab don't end up in the clip. Games are detected through Steam, Lutris, Heroic, Faugus, Bottles
  and umu/Proton. Launchers (EA app, Rockstar, Ubisoft Connect, Battle.net, `*launcher` …) are ignored.
- **Capture modes:**
  - **Auto** (default): Instant Replay and recording capture the desktop. When a game starts
    they switch to the game window, and when the game closes they switch back. The switch is
    seamless: the encoders keep running and only get a new frame source. A clip saved right
    after the game started therefore still contains the desktop part before it, and a
    recording stays one continuous file. Different sizes and aspect ratios are letterboxed on
    the GPU into a fixed output size (the monitor resolution, or the preset limit).
  - **Game only**, **Desktop**, **Window** and **Region**.
- **Clips per game.** Clips are saved to `<output>/<Game name>/…`. The name comes from the launcher's
  library (Steam appmanifest / non-Steam shortcuts, Lutris `GAME_NAME`, Heroic library cache,
  Faugus `games.json`).
- **Instant replay** with a RAM (`/dev/shm`) or disk ring buffer. Saving is a lossless concat, so it
  takes a fraction of a second.
- **Recording** with its own codec, preset and output folder, separate from instant replay.
- **Zero-copy GPU pipeline (VA-API, AMD/Intel):** KWin hands out DMA-BUFs. They are imported into
  VA-API, then colour-converted, scaled and encoded on the GPU, so no pixel goes through the CPU.
  Measured on an RX 6700 with replay + recording at 1080p60: about 8 % of one CPU core,
  against about 75 % for the CPU copy path.
- **Encoders:** H.264, HEVC, AV1 and VP9 via VA-API, NVENC, AMF, Quick Sync, Vulkan Video or CPU
  (x264/x265/SVT-AV1/libvpx). Working encoders are probed at start-up. Non-VA-API encoders get
  frames through the CPU path; GPU frames are downloaded automatically when needed.
- **Presets:** 480p, 720p and 1080p (60 fps, VBR), plus **Custom** (codec, encoder, resolution, fps,
  CBR/VBR/constant quality, bitrate, speed, keyframe interval, 10-bit, container).
- **Audio:** any number of outputs (sink monitors) and inputs (microphones), each with its own
  volume. They can be mixed into one track or kept as one named track per device. Codecs: AAC,
  Opus or FLAC.
- **FireShare upload** ([FireShare](https://github.com/ShaneIsrael/fireshare), self-hosted):
  - Clips are uploaded with an *upload token* (FireShare → Settings → Security → Upload Tokens),
    so no password is stored.
  - Saved clips can be uploaded automatically, and finished recordings optionally too.
  - The share link (`<server>/watch/<id>`) is copied to the clipboard right away.
  - Clips are linked to the game in FireShare's library (if FireShare knows the game), with an
    optional folder per game.
  - The upload is chunked (8 MB), so it also works behind proxies with body limits; MKV is
    remuxed losslessly to MP4.
  - A toast when an upload starts and one when it finishes (with the link). Live progress is
    shown as a progress bar in the overlay's Replay tile; the CLI shows a text progress bar.
    Several clips are queued and uploaded one after another.
  - In the overlay: *Upload last clip* / *Copy link*. CLI: `rust-screen-recorder upload <file> [game]`.
- **Video overlays** (*Global Settings → Overlay*): a 16:9 monitor preview with one slot per
  corner. Overlays are burned into recordings and clips, composited on the GPU in the zero-copy
  path and in software on the CPU path. Size, opacity and edge distance are set per corner.
  - **Keyboard & mouse:** the whole keyboard or the gaming half (Esc–5/T/G/B, modifiers,
    space), with DE/US labels. A mouse next to it shows left, right and middle click, scroll
    up/down and both side buttons. On Wayland this reads `/dev/input`, which needs the `input`
    group: run `sudo usermod -aG input $USER` once and log in again.
  - **Webcam:** any V4L2 camera, optionally mirrored.
  - **Website:** any URL (alerts, chat, widgets), rendered by a headless Chromium-based browser
    (Chromium, Chrome, Brave, Edge, Vivaldi) with a transparent background. An editor shows the
    page; drag with the mouse to cut out the part that should be visible.
- **In-game notifications:** Instant Replay on/off, clip saved (with the real clip length),
  recording started/saved and game detected. They are drawn on a layer-shell overlay surface above
  fullscreen games, never take focus and let clicks through.
- **Overlay** (Alt+Z) is the whole UI; there is no separate settings window. Every setting exists
  in exactly one place: the Replay and Recording tiles have ⚙ for their own settings, the
  Capture tile holds the capture mode, and **⚙ Global Settings** (top right) holds General
  (monitor, region, cursor, notifications, GPU, autostart), Hotkeys, Games and Status.
  Opening it dims the screen at once while the UI slides in. You never have to leave the game.
  Tray icon, desktop notifications and global hotkeys via KGlobalAccel; the hotkeys work while
  a game has focus.

## Requirements

- **KDE Plasma 6** on Wayland (KWin provides the portal-free screencast interface)
- **PipeWire**, **FFmpeg** (the `ffmpeg`/`ffprobe` tools and the libraries)
- For zero-copy GPU encoding: a VA-API driver (Mesa for AMD/Intel)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/wildesPepega/rust-screen-recorder/main/install.sh | bash
```

The script:
- downloads the latest release and verifies its SHA-256 checksum
- installs to `~/.local/bin` (no root needed)
- sets up the launcher entry that KWin checks for portal-free capture
- starts the recorder; press **Alt+Z** for the overlay

Options:

```sh
curl -fsSL https://raw.githubusercontent.com/wildesPepega/rust-screen-recorder/main/install.sh | bash -s -- --uninstall
curl -fsSL https://raw.githubusercontent.com/wildesPepega/rust-screen-recorder/main/install.sh | bash -s -- --version v0.2.0
curl -fsSL https://raw.githubusercontent.com/wildesPepega/rust-screen-recorder/main/install.sh | bash -s -- --no-start
```

`--uninstall` keeps your settings and clips.

**Automatic updates:** at every start the recorder checks GitHub for a new release, verifies it,
installs it and restarts itself – before anything is captured, so no replay is lost. A toast
tells you about the new version. You can turn this off in *Global Settings → General → Updates*,
or update by hand with `rust-screen-recorder update`. Development builds (run from `target/`) and
read-only installs (e.g. a distribution package in `/usr/bin`) are never overwritten; they only
get a notice.

The release binary is built on Arch Linux against **FFmpeg 9 (libavcodec 63)**, PipeWire 1.x and
libva 2. With other FFmpeg versions, build from source: the bindings are generated at build
time, so a source build always matches the system FFmpeg.

On the very first start KWin may not yet know the launcher entry. If the overlay says "screen
capture not permitted", run `rust-screen-recorder ctl quit` and start it once more.

## Build from source

Needs Rust (stable), clang/libclang, and the FFmpeg, libva, PipeWire and Wayland development
files (on Arch they are included in the normal packages).

```sh
cargo build --release
./target/release/rust-screen-recorder          # starts the recorder in the background (again: shows the overlay)
```

On first start the app writes its launcher entry
`~/.local/share/applications/rust-screen-recorder.desktop`. KWin grants the screencast interface
based on the `X-KDE-Wayland-Interfaces` key in that file, and checks it against the binary path in
`Exec=`. If you move the binary, start it once from the new location (or run
`rust-screen-recorder install`), then restart the daemon.

## Default hotkeys

| Action                   | Key           |
|--------------------------|---------------|
| Overlay                  | Alt+Z         |
| Save instant replay      | Alt+F10       |
| Instant replay on/off    | Alt+Shift+F10 |
| Start/stop recording     | Alt+F9        |

## CLI

```
rust-screen-recorder daemon | settings | overlay | install | uninstall | update | upload <file>
rust-screen-recorder ctl save-replay | toggle-replay | toggle-recording | toggle-overlay | status | reload | quit
```

`ctl status` also lists all windows and shows which ones are detected as games.

## Files

- Config: `~/.config/rust-screen-recorder/config.toml`
- Logs: `~/.cache/rust-screen-recorder/` (`daemon.log`, `ffmpeg-replay.log`, `ffmpeg-recording.log`)

## Architecture

```
GPU path (VA-API):
KWin ─zkde_screencast─▶ PipeWire (DMA-BUF, LINEAR) ─▶ hwmap→VA surface ─▶ VPP copy (NV12) ─▶ FrameSlot
   FrameSlot ─▶ [encoder thread] scale_vaapi ─▶ h264/hevc/av1_vaapi ─▶ NUT pipe ─▶ ffmpeg (+audio, mux) ─▶ file / replay segments

CPU path (other encoders / fallback):
KWin ─▶ PipeWire (memfd BGRx) ─▶ FrameSlot ─▶ raw pipe ─▶ ffmpeg (upload/encode + audio) ─▶ file / segments

KWin script (runtime, via D-Bus) ─▶ window list + PIDs ─▶ game detection (/proc environ + launcher libraries)
Toasts: own Wayland connection ─▶ zwlr_layer_shell overlay surface (tiny-skia rendering)
```

Building needs the FFmpeg and libva development headers plus libclang: the bindings are
generated at build time (`build.rs`), so they always match the installed FFmpeg.

## License

GPL-3.0-or-later – see [LICENSE](LICENSE).
`protocols/zkde-screencast-unstable-v1.xml` is from KDE's plasma-wayland-protocols
(LGPL-2.1-or-later, © Aleix Pol Gonzalez).
