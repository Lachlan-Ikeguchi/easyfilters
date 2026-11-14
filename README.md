# EasyFilters

A Linux virtual camera written in Rust (OpenCV + ONNX + iced) that applies
real-time filters to your webcam feed. Any application — Firefox, Discord,
Zoom, interview platforms — can select "EasyFilters Virtual Camera" as its
camera and receive an already-filtered stream.

## Features

- Virtual camera published two ways: a v4l2loopback device (`/dev/videoN`,
  card label "EasyFilters Virtual Camera") for V4L2 apps, and a native
  PipeWire `Video/Source` camera node for PipeWire-based apps (browsers via
  the portal, Snapshot, cosmic camera). Disable the latter with
  `--no-pipewire`.
- Filter modes: `passthrough` (unfiltered), `overlay` (person kept, background
  dimmed/tinted), `mask` (grayscale person mask), `blur` (background
  Gaussian-blurred), `replace` (background = image or solid #003200; the
  default mode).
- Per-frame person segmentation with the bundled MediaPipe Selfie
  Segmentation ONNX model (Apache-2.0), temporal mask smoothing (EMA) and
  feathered mask edges.
- Optional, capped gaze correction ("eye contact"): MediaPipe BlazeFace +
  Face Mesh shift the iris a small, hard-capped amount toward the camera.
  Off by default.
- iced GUI (software renderer, no GPU needed) with a live preview straight
  from the compositing stage, plus a system-tray icon (D-Bus
  StatusNotifierItem, no gtk/appindicator needed).
- Runs in the background: closing the window hides it to the tray, tray
  "Show/Hide window" re-opens it, `--start-minimized` starts tray-only,
  XDG autostart or a NixOS user service launches it at login, and a
  single-instance lock keeps a second launch from fighting over the camera.
- Settings persist in `$XDG_CONFIG_HOME/easyfilters/config.toml`
  (`~/.config/easyfilters/config.toml` by default), written atomically on
  every change.

## Build with nix

This repo is a nix flake. On any machine with nix installed:

```sh
nix build .#default              # builds ./result/bin/easyfilters (fully offline: deps vendored from Cargo.lock)
./result/bin/easyfilters --list  # or: nix run .#default -- --list
nix flake check                  # builds the package + smoke test
nix develop                      # dev shell: plain `cargo build` works, no manual
                                 # OPENCV_*/LIBCLANG_PATH exports needed; also has
                                 # v4l2-ctl, ffmpeg and python3+cv2 for the scripts
```

On plain cargo builds (outside `nix develop`) the opencv crate needs
`OPENCV_INCLUDE_PATHS`, `LIBCLANG_PATH` and `LD_LIBRARY_PATH` pointing at the
system opencv/clang — see the comments in `Cargo.toml`, or just use the flake.

## System setup: the virtual camera device

The v4l2loopback kernel module must be loaded once, with the device labeled
"EasyFilters Virtual Camera":

```sh
sudo scripts/setup-virtual-camera.sh        # --status / --dry-run / --uninstall also available
```

On NixOS, do it declaratively instead — add this repo as a flake input and:

```nix
# configuration.nix
{
  easyfilters.nixosModules.default # loads v4l2loopback with
  easyfilters.enable = true;       # card_label="EasyFilters Virtual Camera", exclusive_caps=0
}                                  # and installs the package system-wide
```

Either way, the user needs `video` group access to `/dev/video*`.

## Usage

```sh
nix build .#default
./result/bin/easyfilters                 # GUI + tray (needs a display; falls back
                                         # to headless automatically without one)
./result/bin/easyfilters --no-gui        # headless: no GUI, no tray
./result/bin/easyfilters --list          # list devices and formats, then exit
./result/bin/easyfilters --self-test frame.jpg   # decode one image, run the full
                                         # pipeline, write mask.png / overlay.png /
                                         # blur.png / replace.png / gaze.png next to it
./result/bin/easyfilters --input /dev/video1 --output /dev/video0 \
    --width 1280 --height 720 --fps 30 --format MJPG
```

Verify the feed: `v4l2-ctl -d /dev/video0 --stream-mmap --stream-count=5` or
`ffmpeg -f v4l2 -input_format mjpeg -i /dev/video0 -frames:v 5 -f null -`.
Set `RUST_LOG=debug` for verbose logging. Ctrl-C / SIGTERM shuts down cleanly.

Apps that enumerate cameras only at startup need a restart after easyfilters
starts. Flatpak apps (Discord, Firefox) already carry `devices=all` and see
the virtual camera; PipeWire clients see it as a source node too
(`exclusive_caps=0`).

### Modes and main flags

```sh
./result/bin/easyfilters --mode blur                          # background Gaussian-blurred
./result/bin/easyfilters --mode blur --blur-strength 25        # stronger blur (sigma at 720p)
./result/bin/easyfilters --mode replace                       # default mode
./result/bin/easyfilters --mode replace --background ~/Pictures/bg.jpg
./result/bin/easyfilters --mode overlay | mask | passthrough
./result/bin/easyfilters --mask-smoothing 0.6                  # EMA alpha for the mask, 0..1
./result/bin/easyfilters --disable                             # force passthrough
./result/bin/easyfilters --gaze                                # enable gaze correction
./result/bin/easyfilters --gaze --gaze-strength 0.8            # stronger (still capped)
./result/bin/easyfilters --no-gaze                             # force gaze off (beats the config)
./result/bin/easyfilters --start-minimized                     # no window, tray only
./result/bin/easyfilters --install-autostart                   # ~/.config/autostart/easyfilters.desktop
./result/bin/easyfilters --uninstall-autostart
```

Model overrides: `--model <path>` / `$EASYFILTERS_MODEL` (segmentation) and
`--face-model <path>` / `$EASYFILTERS_FACE_MODEL` (face mesh) default to the
bundled models in `assets/`. `--config <path>` selects another config file.

### GUI and tray

The window has: filtering on/off, mode picker, blur strength and mask
smoothing sliders, gaze-correction toggler + strength slider, close-to-tray
toggler, background image path (Enter to apply), and input camera /
resolution / fps (Apply restarts the stream live). The status line shows the
actually-negotiated resolution/fps and per-stage timings. Every change takes
effect on the next frame and is saved to the config.

The tray menu has "Toggle filtering", "Toggle gaze correction",
"Show/Hide window" and "Quit". Hiding the window is a real hide on every
platform — the GUI runs as an iced *daemon*, so "hide" destroys the window
(`window::close`; winit can't unmap a Wayland window, only minimize) and
"Show window" re-opens a fresh one; the process and the filtering keep
running in the background either way.

Use the **nix-wrapped** binary (`./result/bin/easyfilters`) for autostart:
the raw `target/release/easyfilters` only works inside `nix develop`
(runtime libs on `LD_LIBRARY_PATH`); outside it a GUI startup failure is
logged and the app keeps filtering headless instead of dying.

### Config file

Precedence: **CLI flag > config file > built-in default**, e.g. `--mode blur`
on the command line beats `mode = "replace"` in the config. Keys mirror the
flags: `enabled`, `mode`, `blur_strength`, `mask_smoothing`, `background`,
`gaze_correction`, `gaze_strength`, `close_to_tray` (default true),
`start_minimized`, `pipewire` (default true), `input`, `output`, `width`,
`height`, `fps`, `format`, `buffers`, `model`, `face_model`.

On NixOS, `easyfilters.autostart = true;` in the flake's module starts the
package as a systemd user service (`--start-minimized`) at graphical login
instead of the XDG entry.

## Performance

Measured on this machine at 1280x720@30 MJPG in a quiet window (release
build, single-threaded): ~29.8 fps, 0 drops, decode ~2.5 ms / infer ~6 ms /
composite ~4.5 ms / encode ~1.7 ms per frame; the gaze stage adds ~4.3 ms
when enabled. The camera input itself fluctuates between ~15 and 30 fps with
system load; the app tracks the input with 0 drops either way. Build with
`--release` — debug-build inference is ~300 ms/frame vs ~7 ms in release.

## Tests

Unit tests use Rust's built-in test harness — no external framework needed:

```sh
cargo test            # runs all #[cfg(test)] unit tests
cargo test -- --nocapture   # show println!/tracing output from tests
```

Coverage:

- `src/frame.rs` — JPEG decode/encode roundtrip, resize, BGR→RGB channel
  order, mask Mat construction/upscaling, overlay & mask rendering, PNG
  write/read roundtrip, Gaussian blur, image loading (all on synthetic
  images, no camera needed).
- `src/composite.rs` — EMA mask smoother (convergence, blending, alpha
  clamping), mask feathering, alpha compositing (full/empty/half mask),
  blur & replace compositing, default background.
- `src/segmentation.rs` — input validation plus a real inference pass over
  the bundled ONNX model asserting a full-size binary mask. Takes a few
  seconds in a debug build.
- `src/main.rs` — config resolution precedence (CLI > config > default).
- `src/pipeline.rs` — `parse_fourcc`, model-path resolution precedence,
  `now_timeval`.
- `src/settings.rs` — `Mode` clap/serde name agreement, preview-slot gating.
- `src/config.rs` — TOML load/save roundtrip, atomic save, missing/malformed
  file handling, default path.
- `src/tray.rs` — `toggle_filtering` / `toggle_gaze` flip the shared flags
  and persist them; the Show/Hide-window flip of `Shared::show_window`.
- `src/lock.rs` — single-instance `flock`: second acquire on the same file
  reports Busy with the holder's pid; lock released on drop.
- `src/autostart.rs` — `.desktop` entry content, idempotent atomic install,
  uninstall reporting, default autostart dir.
- `src/gaze.rs` — eye geometry from synthetic 478-landmark sets (centering,
  direction, hard cap, strength scaling, roll/blink/jitter gates), EMA +
  deadband smoother, `apply_shift` remap on a synthetic iris disc, plus real
  inference passes over both bundled face models.
- `src/gui.rs` — device-field parsing (`WxH`).
- `src/v4l2.rs` is not unit-tested: it drives real `/dev/video*` nodes.
  Exercise it via `--list` and `--self-test` instead.

## Layout

```
src/        pipeline (capture → segment → composite → loopback + PipeWire),
            GUI, tray, config, autostart, single-instance lock
assets/     bundled ONNX models (Apache-2.0) + licenses + app icon
scripts/    setup-virtual-camera.sh (v4l2loopback), debug_camera.py
examples/   ONNX model I/O probes (dump_model.rs, probe_face.rs)
packaging/  .desktop entry; flake.nix also installs it + the hicolor icon
docs/       idea.md — design doc
```
