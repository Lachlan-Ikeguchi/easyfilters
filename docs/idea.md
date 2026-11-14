# EasyFilters — Idea & Design Doc

## Vision

An always-on background filter delivered as a **virtual camera device** on Linux.
Any application — Firefox, Discord, Zoom, interview platforms — can select
"EasyFilters" as its camera and receive an already-filtered feed. Because the
filtering happens at the virtual device level, the user is guaranteed to have
background filtering active in every meeting, with no per-app plugins or
settings to remember.

EasyFilters is positioned as the **video counterpart to
[EasyEffects](https://github.com/wwmm/easyeffects)**: EasyEffects filters
microphone audio at the PipeWire level, EasyFilters filters camera video the
same way. The two apps are designed to sit side by side in the system tray.

## MVP scope

- A virtual camera device (`/dev/videoN`, labeled e.g. "EasyFilters Virtual
  Camera") that shows up in the camera picker of conferencing apps.
- Real-time **background blur** and **background replace** (static image).
- A small GUI (iced) with a live preview and controls: on/off, filter mode,
  blur strength, background image, camera source, output resolution/fps.
- A system tray icon (tray-item) for quick toggling without opening the GUI.
- Persistent settings in a TOML config file.

## Out of scope (for now)

- Full gaze redirection beyond the capped landmark-based pupil shift
  (RTX/Maxine-class — see "Risks").
- Beauty filters, avatars, overlays, segmentation of anything but the person.
- Audio processing (that's EasyEffects' job — the two apps complement each
  other), multi-camera support, streaming-key style scenes.

## Architecture

```
real camera
   │  capture (pipewire crate, MJPEG/YUYV → RGB)
   ▼
frame ──► downscale (e.g. 640x360)
   │          │
   │          ▼
   │     ONNX segmentation model (tract-onnx)
   │     MediaPipe selfie segmentation, landscape 144x256
   │          │
   │     person mask ──► upscale to full res ──► temporal smoothing (EMA)
   │          │
   ▼          ▼
full-res frame + mask ──► composite (opencv): blur or replace background,
                          feather mask edges
   │
   ▼
encode (YUYV/RGB) ──► write to v4l2loopback device ──► apps read /dev/videoN
   │
   └──────────────► live preview in the iced GUI
```

### Stages

1. **Capture** — read frames from the selected camera via the `pipewire` crate
   (PipeWire exposes cameras as nodes; V4L2 read is the fallback path).
2. **Inference** — run MediaPipe Selfie Segmentation (ONNX export, landscape
   model, 144x256 input) through `tract-onnx` on a downscaled frame. The model
   is small enough for real-time CPU inference on a desktop. The mask is
   upscaled to full resolution and smoothed over time (exponential moving
   average) to avoid flicker.
3. **Compositing** — OpenCV: either Gaussian-blur the background region
   (strength configurable) or replace it with a user-chosen image, with
   feathered mask edges.
4. **Output** — write composited frames to a `v4l2loopback` device configured
   with `exclusive_caps=0` and a friendly `card_label` (done: raw ioctls on
   top of `v4l2r`, see `src/v4l2.rs`). PipeWire exposes V4L2 devices as video
   sources, so PipeWire-aware apps (Firefox) and V4L2-based apps (Discord,
   Chrome) all see it. The filtered feed is additionally published as a native
   PipeWire `Video/Source` camera node (`src/pwsource.rs`) for PipeWire-only
   consumers.
5. **GUI** — iced app showing the preview straight from the compositing stage
   (not by re-reading the loopback device), plus the controls listed above.
6. **Tray** — `tray-item` for a one-click enable/disable and "open settings".
7. **Config** — `~/.config/easyfilters/config.toml` via serde/toml.
8. **Logging** — tracing + tracing-subscriber; `clap` for CLI flags (e.g.
   `--config`, `--no-gui`).

## Performance targets

- 720p30 on CPU is the target; the segmentation model runs on a downscaled
  frame and only the mask is upscaled, which keeps the CPU budget small.
- 1080p30 is a stretch goal requiring a GPU inference path (wgpu / ONNX GPU
  delegate / OpenCV DNN with CUDA).
- Policy: drop frames rather than build up a queue — latency matters more than
  smoothness in a meeting.

## Dependency mapping (Cargo.toml)

| Crate | Role |
|---|---|
| `v4l2r` (+ raw ioctls) | camera capture, loopback output, MMAP queues |
| `pipewire` | native PipeWire camera source node |
| `tract-onnx` | ONNX inference (segmentation, face models) |
| `opencv` | frame decode/resize/encode, compositing, blur, image loading |
| `iced` | GUI (tiny-skia software renderer) |
| `tray-item` | system tray (ksni / D-Bus StatusNotifierItem) |
| `serde` / `toml` | config |
| `tracing` / `tracing-subscriber` | logging |
| `clap` | CLI |
| `anyhow` | error handling |

## Risks & open questions

- **v4l2loopback kernel module**: must be installed out-of-band (dkms package on
  most distros). The app should detect its absence at startup and print clear
  install instructions instead of failing cryptically.
- **App enumeration quirks**: some apps enumerate cameras only at startup, so
  enabling EasyFilters may require restarting the app once. Document this.
- **Eye contact correction is capped, not full gaze redirection.** Production-
  quality gaze redirection (NVIDIA Maxine) needs an RTX GPU and is proprietary;
  CPU-feasible alternatives are research-grade. The app implements only a
  subtle landmark-based pupil shift as an optional, capped, opt-in effect
  (`src/gaze.rs`).
- **Model licensing**: MediaPipe models are Apache 2.0 — fine to bundle or
  download at first run.
- **Pure-PipeWire alternative**: instead of v4l2loopback, a custom PipeWire
  source node is more native but less compatible with apps that still enumerate
  V4L2 devices. The app publishes both: v4l2loopback for V4L2 apps, the native
  node for PipeWire consumers.
