# EasyFilters

A pipewire and openCV virtual camera app written in rust using the iced framework to apply filters to any software using the camera.

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

On NixOS, enable the virtual camera declaratively instead of running the
setup script — add this repo as a flake input and:

```nix
# configuration.nix
{
  easyfilters.nixosModules.default # loads v4l2loopback with
  easyfilters.enable = true;       # card_label="EasyFilters Virtual Camera", exclusive_caps=0
}                                  # and installs the package system-wide
```

On non-NixOS distros keep using `sudo scripts/setup-virtual-camera.sh`.
Either way, the user needs `video` group access to `/dev/video*`.

## Milestone 1: unfiltered passthrough

Reads the real camera and writes the frames through to the v4l2loopback
virtual camera, unfiltered — proves the virtual camera shows up and works.

One-time system setup (installs/configures the v4l2loopback module and
labels the device "EasyFilters Virtual Camera"):

```sh
sudo scripts/setup-virtual-camera.sh        # --status / --dry-run / --uninstall also available
```

Build and run (on machines where libclang is not in the default linker path,
export `LIBCLANG_PATH` to the clang-lib store path first):

```sh
cargo build
./target/debug/easyfilters                 # auto-detects input camera and virtual output
./target/debug/easyfilters --list          # list devices and formats
./target/debug/easyfilters --input /dev/video1 --output /dev/video0 --width 1280 --height 720 --fps 30 --format MJPG
```

Verify the feed: `v4l2-ctl -d /dev/video0 --stream-mmap --stream-count=5` or
`ffmpeg -f v4l2 -input_format mjpeg -i /dev/video0 -frames:v 5 -f null -`.
Set `RUST_LOG=debug` for verbose logging. Ctrl-C / SIGTERM shuts down cleanly.

## Milestone 2: segmentation mask preview

Runs the MediaPipe Selfie Segmentation ONNX model (bundled in
`assets/selfie_segmentation_landscape.onnx`, Apache-2.0) via tract-onnx on
every frame and renders the person mask into the virtual camera feed, so you
can watch the segmentation live in any camera app before the GUI exists:

```sh
cargo build --release                       # release matters: ~7 ms infer vs ~300 ms debug
./target/release/easyfilters --mode overlay   # person kept, background dimmed/tinted
./target/release/easyfilters --mode mask      # pure grayscale mask
./target/release/easyfilters --mode passthrough  # milestone 1 behavior, unfiltered

Running with no `--mode` defaults to `--mode replace` (solid #003200
background) since milestone 3.
./target/release/easyfilters --self-test frame.jpg  # writes mask.png + overlay.png next to it
```

`--model <path>` (or `EASYFILTERS_MODEL`) overrides the bundled model. Measured
on this machine at 1280x720@30 MJPG: ~29.8 fps, 0 drops, decode ~3 ms /
infer ~7 ms / encode ~4 ms per frame, single-threaded. Flatpak apps (Discord,
Firefox) already carry `devices=all` and see the virtual camera; PipeWire
clients see it as a source node too (`exclusive_caps=0`). Apps that enumerate
cameras only at startup need a restart after easyfilters starts.

Build note for this nix-based machine: the opencv crate needs the
`clang-runtime` feature plus `OPENCV_INCLUDE_PATHS=/usr/include`,
`LIBCLANG_PATH=<nix clang-lib>/lib` and `LD_LIBRARY_PATH=<same>` (see
Cargo.toml comments) — or just use `nix develop` / `nix build`, which set
all of these up automatically (see "Build with nix" above).

## Milestone 3: compositing — background blur & replace

Turns the segmentation mask into real filters in the virtual camera feed,
with temporal smoothing (EMA on the mask, `--mask-smoothing`, default 0.6)
and feathered mask edges:

```sh
./target/release/easyfilters --mode blur                          # background Gaussian-blurred
./target/release/easyfilters --mode blur --blur-strength 25        # stronger blur (sigma at 720p)
./target/release/easyfilters --mode replace                       # background = solid #003200 (the default mode)
./target/release/easyfilters --mode replace --background ~/Pictures/bg.jpg
./target/release/easyfilters --self-test frame.jpg                 # also writes blur.png + replace.png
```

Measured on this machine at 1280x720@30 MJPG in a quiet window: 29.8 fps,
0 drops, decode ~2.5 ms / infer ~6 ms / composite ~4.5 ms / encode ~1.7 ms
per frame, single-threaded (release build). The camera input itself
fluctuates between ~15 and 30 fps with system load; the app tracks the input
with 0 drops either way.

## Milestone 4: GUI + tray + config

The pipeline now runs as a background thread and the app is a small iced
window (software renderer, no GPU needed) with a live preview taken straight
from the compositing stage, plus a system-tray icon:

```sh
./target/release/easyfilters                 # GUI + tray (needs a display)
./target/release/easyfilters --no-gui        # headless, milestone 1-3 CLI behavior
./target/release/easyfilters --config /tmp/x.toml   # use another config file
```

Controls in the window: filtering on/off, mode (`passthrough`/`overlay`/
`mask`/`blur`/`replace`), blur strength, mask smoothing, background image path
(Enter to apply), and input camera / resolution / fps (Apply restarts the
stream live). Every change takes effect on the next frame and is saved to the
config. Without `DISPLAY`/`WAYLAND_DISPLAY` the app falls back to headless
automatically.

Settings persist in `$XDG_CONFIG_HOME/easyfilters/config.toml`
(`~/.config/easyfilters/config.toml` by default), written atomically on every
change. Precedence: **CLI flag > config file > built-in default**, e.g.
`--mode blur` on the command line beats `mode = "replace"` in the config.
`--disable` forces passthrough. The tray menu (a D-Bus StatusNotifierItem, no
gtk/appindicator needed) has "Toggle filtering", "Toggle gaze correction",
"Show/Hide window" (milestone 6) and "Quit".

Build notes: the GUI uses iced with the `tiny-skia` renderer only (no
wgpu/shaderc); the tray uses `tray-item`'s `ksni` feature, which links
`libdbus` at build time (provided by the nix flake; on a plain host set
`PKG_CONFIG_PATH` to a `dbus-1.pc`). winit's wayland/x11 backends need
`libwayland`/`libxkbcommon` at runtime — again provided by the flake.

## Milestone 5: gaze correction (capped, opt-in pupil shift)

Optional "eye contact" effect: MediaPipe BlazeFace (face detection) +
Face Mesh V2 (478 landmarks incl. iris), both Apache-2.0 and bundled in
`assets/`, find the face and each iris, and shift the iris texture a small,
hard-capped amount toward the eye center / webcam — so in the virtual feed
you appear to look at the camera. It is **off by default** (the one effect
that can look uncanny), gated on roll/blink/face-size/presence/jitter, and
capped at `min(0.25 * eye_width, 3.5 px * h/720)` per axis:

```sh
./target/release/easyfilters --no-gui --gaze                 # headless with gaze on
./target/release/easyfilters --gaze --gaze-strength 0.8      # stronger (still capped)
./target/release/easyfilters --no-gaze                       # force off (beats the config)
./target/release/easyfilters --self-test frame.jpg           # also writes gaze.png + prints per-eye px shift
```

Config: `gaze_correction = true`, `gaze_strength = 0.5`. GUI: "Gaze
correction" toggler + strength slider; tray: "Toggle gaze correction".
`--face-model <path>` (or `EASYFILTERS_FACE_MODEL`) overrides the bundled
face-mesh model. Applies to `overlay`/`blur`/`replace` (not `mask`/
passthrough) and is copied through the person mask only.

Measured at 1280x720@30 MJPG with gaze on: 29.8 fps, 0 drops,
gaze stage ~4.3 ms/frame (detector every frame, mesh every 3rd, remap per
frame) — same fps as milestone 4 with gaze off (gaze_ms = 0).

## Milestone 6: run in the background, launch at login, packaging

Closing the GUI window now **hides it to the tray** instead of quitting
(`close_to_tray = true` by default) — filtering keeps running and the tray
is the control surface. The tray menu gained "Show/Hide window" (plus the
existing toggles and "Quit", which still stops the app). Start with the
window hidden (tray only) via `--start-minimized`:

```sh
./target/release/easyfilters --start-minimized   # no window, tray only, filtering runs
./target/release/easyfilters --no-gui             # fully headless (servers), unchanged
```

Launch at login via an XDG autostart entry:

```sh
nix build && ./result/bin/easyfilters --install-autostart   # writes ~/.config/autostart/easyfilters.desktop
./result/bin/easyfilters --uninstall-autostart
```

Use the **nix-wrapped** binary (`./result/bin/easyfilters`) for autostart:
the raw `target/release/easyfilters` only works inside `nix develop`
(runtime libs on `LD_LIBRARY_PATH`); outside it the GUI can fail to start
— in that case the app now logs the error and keeps filtering headless
instead of dying. Note: hiding the window is a real hide on every platform
— the GUI runs as an iced *daemon*, so "hide" destroys the window
(`window::close`; winit can't unmap a Wayland window, only minimize) and
tray "Show window" re-opens a fresh one; the process and the filtering
keep running in the background either way.

On NixOS, `easyfilters.autostart = true;` in the flake's module starts the
package as a systemd user service (`--start-minimized`) at graphical login
instead. A single-instance lock (`$XDG_RUNTIME_DIR/easyfilters.lock`) makes
a second launch print "already running (pid N)" and exit 0 instead of
fighting over the camera with a cryptic EBUSY. Packaging: the nix package
(0.2.0) now ships `share/applications/easyfilters.desktop` + a hicolor SVG
icon, and the wrapped binary carries the X11 runtime libs winit dlopens.

Smaller polish in the same milestone: the GUI status line shows the
actually-negotiated resolution/fps (the startup log prints
`requested=...` next to the negotiated values), `--format` is validated
before any device is opened, and a camera/loopback held by another app logs
a targeted "device busy" hint instead of only the generic retry message.

Measured at 1280x720@30 MJPG in a quiet window: `--no-gui --mode blur` and
`--start-minimized` both run at ~29.8 fps, 0 drops — the milestone 4/5 bar
unchanged.

## Tests

Unit tests use Rust's built-in test harness — no external framework needed:

```sh
cargo test            # runs all #[cfg(test)] unit tests
cargo test -- --nocapture   # show println!/tracing output from tests
```

Coverage today:

- `src/frame.rs` — JPEG decode/encode roundtrip, resize, BGR→RGB channel
  order, mask Mat construction/upscaling, overlay & mask rendering, PNG
  write/read roundtrip, Gaussian blur (edge softening, solid-image identity),
  image loading (all on synthetic images, no camera needed).
- `src/composite.rs` — EMA mask smoother (convergence, blending, alpha
  clamping), mask feathering, alpha compositing (full/empty/half mask),
  blur & replace compositing, default gradient background.
- `src/segmentation.rs` — input validation (dimensions, buffer length) plus
  a real inference pass over the bundled ONNX model asserting a full-size
  binary mask. This one takes a few seconds in a debug build.
- `src/main.rs` — config resolution precedence (CLI > config > default).
- `src/pipeline.rs` — `parse_fourcc`, `resolve_model` precedence, `now_timeval`.
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
  deadband smoother, `apply_shift` remap on a synthetic iris disc (moves the
  disc, respects a zero mask, no-ops on tiny shifts), plus real inference
  passes over both bundled face models.
- `src/gui.rs` — device-field parsing (`WxH`).
- `src/v4l2.rs` is not unit-tested: it drives real `/dev/video*` nodes.
  Exercise it via `--list` and `--self-test` instead.
