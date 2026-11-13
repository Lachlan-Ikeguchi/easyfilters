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

- Full gaze redirection beyond the capped landmark-based pupil shift of
  milestone 5 (RTX/Maxine-class — see "Risks").
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
   with `exclusive_caps=1` and a friendly `card_label`. PipeWire exposes V4L2
   devices as video sources, so PipeWire-aware apps (Firefox) and V4L2-based
   apps (Discord, Chrome) all see it. Note: the `v4l2r` crate is read-oriented;
   writing to the loopback device will need the `v4l` crate or raw ioctls.
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

| Crate (current) | Role |
|---|---|
| `pipewire` | camera capture |
| `opencv` | frame conversion, compositing, blur |
| `iced` / `iced_aw` | GUI |
| `tray-item` | system tray |
| `tokio` | async runtime for the capture/output pipeline |
| `serde` / `serde_json` / `toml` | config |
| `tracing` / `tracing-subscriber` | logging |
| `clap` | CLI |
| `anyhow` | error handling |

**To add:** `tract-onnx` (or `ort`) for ONNX inference, `v4l` (or ioctls) for
writing to the loopback device, and an image-loading crate for background
images.

## Risks & open questions

- **v4l2loopback kernel module**: must be installed out-of-band (dkms package on
  most distros). The app should detect its absence at startup and print clear
  install instructions instead of failing cryptically.
- **App enumeration quirks**: some apps enumerate cameras only at startup, so
  enabling EasyFilters may require restarting the app once. Document this.
- **Eye contact correction is capped, not full gaze redirection.** Production-
  quality gaze redirection (NVIDIA Maxine) needs an RTX GPU and is proprietary;
  CPU-feasible alternatives are research-grade. Milestone 5 implements only a
  subtle landmark-based pupil shift as an optional, capped, opt-in effect —
  see `plan/milestone5.md`.
- **Model licensing**: MediaPipe models are Apache 2.0 — fine to bundle or
  download at first run.
- **Pure-PipeWire alternative**: instead of v4l2loopback, a custom PipeWire
  source node is more native but less compatible with apps that still enumerate
  V4L2 devices. Decision for MVP: v4l2loopback, revisit later.

## Milestones

1. **Plumbing**: capture the real camera and write it through to the loopback
   device unfiltered — proves the virtual camera shows up and works in
   Firefox/Discord. This is the riskiest part; do it first.
2. **Segmentation**: integrate the ONNX model, show the person mask in the
   preview.
3. **Compositing**: blur + background replace with temporal smoothing.
4. **GUI + tray + config**: full controls, persistent settings, tray toggle.
5. **Gaze correction**: subtle, capped, landmark-based pupil shift (optional,
   opt-in) so the user appears to look at the camera — see `plan/milestone5.md`.
6. **Polish**: run in the background (close-to-tray, `--start-minimized`,
   single-instance lock), startup behavior (launch at login), packaging
   (`.desktop` + icon, NixOS autostart), any leftover
   resolution/fps/camera-selection rough edges — see `plan/milestone6.md`.
