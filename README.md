# EasyFilters

A pipewire and openCV virtual camera app written in rust using the iced framework to apply filters to any software using the camera.

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
Cargo.toml comments).

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
- `src/main.rs` — `parse_fourcc`, `model_path` precedence, `now_timeval`.
- `src/v4l2.rs` is not unit-tested: it drives real `/dev/video*` nodes.
  Exercise it via `--list` and `--self-test` instead.
