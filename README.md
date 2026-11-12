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
