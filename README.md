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
./target/release/easyfilters --mode passthrough  # milestone 1 behavior (default)
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
