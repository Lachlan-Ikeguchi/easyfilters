# Milestone 1 — Plumbing: unfiltered passthrough to the virtual camera

Goal (from `docs/idea.md`): capture the real camera and write it through to the
v4l2loopback device **unfiltered**, proving the virtual camera shows up and
works in Firefox/Discord. This is the riskiest part of the project, so it goes
first. Repo state: bare skeleton (`src/main.rs` is hello-world) on branch
`feat/unfiltered_passthrough`, plus `scripts/setup-virtual-camera.sh` (see
"System setup" below).

## Environment findings (verified on this machine, 2026-11-12)

- Running kernel is now **7.3.3-1-default** (the reboot into the new kernel
  happened) and **v4l2loopback 0.15.4 is loaded** (`lsmod`), built at
  `/usr/lib/modules/7.3.3-1-default/updates/v4l2loopback.ko`. The old
  kernel-mismatch blocker is gone; live output verification is possible now.
- Device numbering changed because the module autoloaded with `video_nr=-1`:
  - `/dev/video0` = **loopback output** ("OBS Virtual Camera",
    `platform:v4l2loopback-000`; Device Caps `0x05200002` = Video Output +
    Streaming + RW; default format BGR4 640x480@30, 2 write buffers)
  - `/dev/video1` = **real camera capture** (uvcvideo, "Integrated Camera"),
    `/dev/video2` = its metadata node, `/dev/media0`
  - `/dev/video10` does **not** exist. Consequence: the app must not hardcode
    node numbers — discover devices by card label / driver name (see Design
    decisions).
- Real camera formats (`v4l2-ctl -d /dev/video1 --list-formats-ext`):
  - **MJPG** 1920x1080@30 down to 320x180@30, incl. 1280x720@30, 960x540@30,
    640x480@30
  - **YUYV** max 640x480@30; 1280x720 only @10fps
- ACL: `user:lachlan:rw-` on all `/dev/video*` nodes — direct V4L2 access works.
- PipeWire 1.6.11 + WirePlumber run as user `lachlan`; camera exposed as
  `v4l2_input.pci-0000_00_14.0-usb-0_5_1.0` (confirmed in `pw-dump`).
  With the currently active `exclusive_caps=1`, the loopback shows as a
  PipeWire *Device* but gets **no source node** → invisible to PipeWire
  clients; V4L2 apps see it. Setting `exclusive_caps=0` is expected to make
  WirePlumber create a `v4l2_output` node too — verify in step 5.
- Toolchain: `v4l2-ctl` and `ffmpeg` installed; `videodev2.h` present;
  cargo/rustc 1.102 nightly (nix profile). `clang` 23.1.4 present, but
  `libclang.so` lives only in the nix store and is not in the ldconfig cache —
  if v4l2r's bindgen build fails to find libclang, set
  `LIBCLANG_PATH=/nix/store/yc2a9854a2y2c8kci88piblc847iq1l4-clang-21.1.8-lib/lib`.
  `modinfo`/`dkms` binaries are not installed (not needed). No browsers or
  Discord on this dev box — real app-compat testing happens on a desktop
  session, not here.
- Autoload configs already in place (from the OBS-style setup):
  `/usr/lib/modules-load.d/v4l2loopback.conf` and
  `/usr/lib/modprobe.d/98-v4l2loopback.conf` → `exclusive_caps=1
  card_label="OBS Virtual Camera"`.

## System setup for end users (done — script)

`scripts/setup-virtual-camera.sh` (verified on this machine with `--status`
and `--dry-run`) removes the manual setup burden: one root run installs the
right per-distro package (zypper `v4l2loopback-kmp-<flavor>` on openSUSE,
`v4l2loopback-dkms` via apt on Debian/Ubuntu, dnf on Fedora, pacman on Arch),
writes `/etc/modprobe.d/99-easyfilters-v4l2loopback.conf` (named 99- so it
overrides earlier confs) plus `/etc/modules-load.d/` for boot autoload,
reloads the module, and verifies the device appears. Defaults:
`card_label="EasyFilters Virtual Camera"`, `exclusive_caps=0`, `video_nr=-1`;
overridable via `--label` / `--exclusive-caps` / `--video-nr`, plus `--status`,
`--uninstall`, `--dry-run`.

## Design decisions

- **One crate does both sides.** `v4l2r 0.0.7` (already in Cargo.toml) exposes
  the full ioctl layer (`s_fmt`, `reqbufs`, `querybuf`, `qbuf`, `dqbuf`,
  `streamon/off`, `enum_fmt`, frame sizes/intervals) for capture *and* output
  queues, plus `bindings` and a re-exported `nix`. **No new dependencies.**
- **Compressed passthrough.** Negotiate **MJPG** on the camera (fallback:
  YUYV) and set the same fourcc on the loopback output queue → frames are a
  pure `memcpy`, zero CPU, zero decode. (Loopback MJPG acceptance is not
  enumerable via v4l2-ctl; prove it with the ffmpeg write test in step 5.)
- **Device discovery, not hardcoded nodes.** Input defaults to the first
  `/dev/videoN` whose driver is `uvcvideo` with capture caps (`--input`
  overrides); output defaults to the `/dev/videoN` whose card label is
  "EasyFilters Virtual Camera" (`--output` overrides). Numbering shifts
  whenever the module autoloads, so labels are the stable handle.
- **Single-threaded blocking pipeline** with `poll()` on the input fd — no
  tokio needed for milestone 1 (kept in deps for later milestones).
- **Drop-frame policy** (per idea doc): if all output buffers are in flight,
  drop the frame instead of queueing latency.
- Non-blocking fds + EAGAIN-tolerant `dqbuf` on both queues; clean
  SIGINT/SIGTERM shutdown via a signal handler setting an `AtomicBool`.
- **`exclusive_caps=0` for the MVP** (setup-script default) so the virtual
  camera is visible to both PipeWire clients and V4L2-only apps. Revisit only
  if a specific app misbehaves.
- **Pure-PipeWire output explored and deferred.** A `PW_DIRECTION_OUTPUT`
  stream is a valid video source node and the `pipewire` 0.10 crate supports
  it (`Stream::connect(Direction::Output, …)` + `process` callback, no kernel
  module, no root), but Firefox (`media.webrtc.camera.allow-pipewire`) and
  Chromium/Electron (`WebRtcPipeWireCamera`) both default to V4L2 for cameras,
  so a pure-PipeWire node is invisible by default in exactly the target apps.
  `pw-v4l2` (LD_PRELOAD shim) can bridge individual V4L2 apps later. Keep
  v4l2loopback for the MVP; revisit after milestone 4.

## Implementation steps

1. **Cargo.toml** — rename package `master` → `easyfilters`; enable `clap`
   `derive` and `tracing-subscriber` `env-filter` features. No new crates.

2. **`src/v4l2.rs`** (new) — thin V4L2 wrapper over `v4l2r`:
   - open device `O_RDWR|O_NONBLOCK`, `querycap` (check VIDEO_CAPTURE /
     VIDEO_OUTPUT device cap for the queue direction), read driver/card
   - enumerate formats/sizes/frame-intervals (`FormatIterator`,
     `enum_frame_sizes`, `enum_frame_intervals`)
   - `s_fmt`/`g_fmt`, `s_parm` (timeperframe for fps)
   - `reqbufs` + `querybuf` + `mmap` per buffer, `streamon`/`streamoff`
   - `qbuf` (MMAP, with monotonic timestamp via `SystemTime`), `try_dqbuf`
     returning `None` on EAGAIN, `poll` helper
   - device discovery helpers: scan `/sys/class/video4linux/*/{name,devnode}`
     or querycap per node; match output by card label, input by driver

3. **`src/main.rs`** — CLI + pipeline:
   - clap derive CLI: `--input` (default: auto-detect uvcvideo capture node),
     `--output` (default: auto-discover by card label "EasyFilters Virtual
     Camera"), `--width`, `--height`, `--fps` (default 30), `--format`
     (fourcc, default auto), `--buffers` (default 4), `--list` (print device
     caps + format tree, exit)
   - tracing-subscriber init with `RUST_LOG` env filter
   - format negotiation: prefer user `--format`, else MJPG → YUYV; default
     1280x720, nearest available size; frame interval closest to `--fps`;
     `s_parm` the chosen fps on the input
   - set the same fourcc/resolution on the loopback output queue
   - loop: `poll input → dqbuf → reclaim finished output buffers → copy into a
     free output buffer → qbuf output → requeue input`; drop + count when no
     free output buffer; log fps/drops every ~2 s
   - if the output device is not found, print a hint to run
     `scripts/setup-virtual-camera.sh` (the app detects the missing module /
     device instead of failing cryptically, per idea doc)

4. **Build** — `cargo build`; fix v4l2r 0.0.7 API mismatches; if bindgen
   cannot locate libclang, export `LIBCLANG_PATH` to the nix clang-lib store
   path noted above.

5. **Verify** (live now — module is loaded):
   - `easyfilters --list` against the auto-detected input (proves capture side
     + format negotiation)
   - `sudo scripts/setup-virtual-camera.sh` once to relabel the loopback
     "EasyFilters Virtual Camera" with `exclusive_caps=0`, then run the
     passthrough in the background and prove live frames on the discovered
     output node with `v4l2-ctl -d <dev> --stream-mmap --stream-count=5` and
     `ffmpeg -f v4l2 -i <dev> -frames:v 5 ...` (this also proves MJPG output)
   - `pw-dump` / `wpctl status` — with `exclusive_caps=0` the loopback should
     now appear as a `v4l2_output` source node for PipeWire clients
   - app-level check on a desktop session: V4L2 app (e.g. `ffplay` or Discord)
     and Firefox camera picker both list "EasyFilters Virtual Camera"

6. **README** — usage section incl. the one-time `sudo
   scripts/setup-virtual-camera.sh`; commit on `feat/unfiltered_passthrough`.

## Risks / open questions

- `exclusive_caps` trade-off is decided for the MVP (0 = visible everywhere);
  flip to 1 via the setup script only if an app grabs the device exclusively.
- Loopback MJPG output is assumed but unproven until the step-5 ffmpeg test;
  raw YUYV path exists if a target app needs it.
- Some apps enumerate cameras only at startup — may need an app restart
  (document in README).
- nix libclang caveat (LIBCLANG_PATH) may be needed for the bindgen build on
  this machine; on normal distros libclang is in the default linker path.
- This dev box has no browsers — app-compat claims are verified via
  v4l2-ctl/ffmpeg/pw-dump here and must be sanity-checked once on a real
  desktop session.
- Setup script requires root once; unsupported distros get manual DKMS
  instructions printed instead of a silent failure.
