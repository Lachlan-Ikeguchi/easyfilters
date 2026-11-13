# Milestone 4 — GUI + tray + config

Goal (from `docs/idea.md`, milestone 4): **full controls, persistent settings,
tray toggle.** An iced GUI shows a live preview straight from the compositing
stage (not by re-reading the loopback device) plus controls — on/off, filter
mode, blur strength, background image, mask smoothing, camera source, output
resolution/fps. A `tray-item` system-tray icon gives a one-click enable/disable
and quit. Settings persist in `~/.config/easyfilters/config.toml`. Headless use
keeps working via `--no-gui` (the milestone 1-3 CLI behavior).

Repo state at planning time: branch `feat/milestone-4`, clean, at `67e32bc`
(milestone 3 complete: `--mode blur|replace` verified live on `/dev/video0` at
1280x720@30, 29.8 fps, 0 drops, decode ~2.5 / infer ~6.2 / composite ~4.4 /
encode ~1.7 ms in release, single-threaded). The capture/filter/output loop is
a monolithic `run()` in `src/main.rs` — M4 turns it into a library-style
pipeline thread driven by shared settings.

## Environment findings (verified on this machine, 2026-11-13)

- Session: **KDE Plasma Wayland** (`kwin_wayland` on `wayland-1`, Xwayland on
  `:1`), `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus` — a D-Bus
  StatusNotifierWatcher exists (Plasma tray), so an SNI tray icon registers.
  Screenshots for verification: ImageMagick `import`/`xdpyinfo` against
  Xwayland `:1`.
- Devices: `/dev/video0` = "EasyFilters Virtual Camera" loopback
  (`exclusive_caps=0`), `/dev/video1` = real uvcvideo camera, `/dev/video2` =
  metadata. Same as M1-M3.
- crates.io reachable (sparse index + static CDN), `cache.nixos.org` reachable
  (nix binary cache — gtk/wayland system packages would be prebuilt if needed).
  12 cores, 15 GiB RAM, 154 GiB free.
- opencv build env on the host: `OPENCV_INCLUDE_PATHS=/usr/include`
  (`opencv4.pc` in `/usr/lib64/pkgconfig`), `LIBCLANG_PATH` and
  `LD_LIBRARY_PATH=/nix/store/5ajx1np9zlq7z6ljqq18sibpzhnjakdw-clang-22.1.8-lib/lib`.
- **No system gtk3 / libappindicator / libpipewire** (`pkg-config` misses
  all three) — this decides two dependency choices below.
- Crate facts (checked against the crates.io index on 2026-11-13):
  - `iced 0.14.0` exists; its `default` features include **both** `wgpu` and
    `tiny-skia`. With `default-features = false` + `["tiny-skia", "image",
    "thread-pool", "wayland", "x11", "web-colors", "crisp", "fira-sans"]` we get
    a pure-Rust software renderer — no wgpu/shaderc/cmake/GPU and no extra
    system libs. 720p30 RGBA preview through a single `image` widget is well
    within CPU budget; wgpu stays the 1080p stretch path.
  - `tray-item 0.10.0` has a **`ksni` feature**: pure-Rust StatusNotifierItem
    over D-Bus (zbus), zero system deps — unlike its `libappindicator` feature
    which needs gtk3 + libappindicator (absent here, and heavy in the nix
    build). KDE Plasma and COSMIC both host an SNI watcher, so ksni works.
  - `serde 1.0.228` + `toml 1.1.2` as planned in `docs/idea.md`.
  - `pipewire`/`tokio` stay unused: capture remains the `v4l2r` MMAP path in a
    plain `std::thread`; iced's `thread-pool` executor covers GUI futures. No
    async runtime needed.

## Design decisions

- **Threading model:** iced event loop on the main thread; the capture loop in
  a spawned `std::thread` (`src/pipeline.rs`); the tray in another
  (`src/tray.rs`). They communicate only through `Arc<Shared>`:
  - `settings: RwLock<Settings>` — `enabled`, `mode`, `blur_strength`,
    `mask_smoothing`, `background: Option<PathBuf>` — read by the pipeline every
    frame (an uncontended RwLock read is ns-cheap), written by GUI/tray.
  - `bg_generation: AtomicU64` — bumped when `background` changes; the pipeline
    reloads/resizes the image only when the generation moved (image load is
    not per-frame).
  - `device_generation: AtomicU64` + `device: RwLock<DeviceParams>` —
    input/output/width/height/fps/format/buffers. The pipeline runs an outer
    loop: when `device_generation` moves, it tears down (stream_off/free) and
    re-opens/re-negotiates, so GUI changes to camera source or resolution take
    effect without an app restart. Renegotiate failure logs a `warn!` and
    retries after 2 s instead of killing the app.
  - `preview: Mutex<Option<PreviewFrame>>` — the pipeline publishes the frame
    it is about to write to the loopback as RGBA8 + (w, h) + seq; the GUI polls
    it on a 33 ms `time::every` subscription and rebuilds the
    `image::Handle::from_rgba` only when seq moved. Publishing is skipped when
    no GUI is attached (`preview_on: AtomicBool`), so `--no-gui` pays nothing.
  - `running: AtomicBool` — set false by SIGINT/SIGTERM (existing handler), by
    tray "Quit", or by the window closing; pipeline and GUI both watch it.
- **Config (`src/config.rs`):** TOML at `$XDG_CONFIG_HOME/easyfilters/config.toml`
  (fallback `$HOME/.config/easyfilters/config.toml`), `--config <path>` to
  override. Every field optional (`#[serde(default)]`); precedence
  **CLI arg > config file > built-in default**. To make that possible the clap
  args become `Option<T>` without `default_value_t`, resolved in one
  `resolve()` step together with the config. GUI/tray changes write the merged
  settings back immediately (create parent dirs), so the next launch starts
  where the last session left off.
- **Modes:** `Mode` moves to `src/settings.rs`, derives `ValueEnum` (clap) +
  `Serialize/Deserialize` (config, lowercase names). New field `enabled:
  bool` (default true) — when false the pipeline byte-copies passthrough
  regardless of `mode` (the tray toggle).
- **Pipeline (`src/pipeline.rs`):** `run()` moves out of `main.rs` mostly
  unchanged — same v4l2 code, drop-frame policy, stats log, decode-error
  drop. Per frame it snapshots the shared settings: `enabled == false` →
  passthrough copy; else current `mode` (overlay/mask keep the raw hard mask;
  blur/replace use the EMA smoother whose alpha follows `mask_smoothing`
  live — the smoother is recreated when alpha changes); `sigma` recomputed
  from `blur_strength` per frame. Background for replace is cached and
  reloaded on `bg_generation` bump. Preview publish happens right before
  `qbuf`. `--self-test` and `--list` stay in `main.rs` (no pipeline needed).
- **GUI (`src/gui.rs`):** iced `application` with a `row![preview, controls]`.
  Preview = `image` widget (or "starting…" text until the first frame).
  Controls: `toggler` enabled, `pick_list` mode, `slider` blur strength
  (1..=40), `slider` mask smoothing (0.0..=1.0), `text_input` + note for the
  background image path, `text_input`s for input device / resolution (WxH) /
  fps that trigger a device restart on submit. Every change updates
  `Shared`, bumps the right generation, and saves the config. A status line
  shows fps/drops/mode from a stats slot the pipeline fills every 2 s.
  Window close → `running = false`. Tray quit → GUI's tick sees it and calls
  `iced::exit()`.
- **Tray (`src/tray.rs`):** `tray-item` with `features = ["ksni"]`. Menu:
  "Enable/Disable filtering" (flips `settings.enabled`, saves config) and
  "Quit" (sets `running = false`). Icon generated in code (32x32 RGBA, green
  rounded square) via `Icon::from_rgba` — no binary asset. Tray failure (no
  watcher on the bus) is a `warn!`, never fatal.
- **CLI:** existing flags unchanged in behavior; `--no-gui` forces headless,
  `--config <path>` added. Default (no `--no-gui`) launches the GUI when
  `DISPLAY` or `WAYLAND_DISPLAY` is set, otherwise falls back to headless with
  an `info!` note — servers keep working.
- **flake.nix:** add `libxkbcommon` and `wayland` to buildInputs/devShell
  (winit's wayland/x11 backends dlopen libxkbcommon and may link libwayland
  at build time; both are prebuilt in the binary cache). Package build stays
  pure-Rust-crates-only otherwise; wrapProgram gains nothing new.
- **No new system deps for the default build path**: tiny-skia + ksni are pure
  Rust, so `nix build`/`nix develop`/`cargo build` all work on this machine.

## Implementation steps

1. `plan/milestone4.md` (this file) — commit as docs.
2. `Cargo.toml`: uncomment/replace the planned deps with
   `serde { derive }`, `toml`, `iced { default-features = false, features =
   ["tiny-skia","image","thread-pool","wayland","x11","web-colors","crisp","fira-sans"] }`,
   `tray-item { version = "0.10", default-features = false, features = ["ksni"] }`.
   `flake.nix`: add `libxkbcommon`, `wayland` to buildInputs + devShell.
   `cargo fetch` to confirm resolution; commit.
3. `src/settings.rs` — `Mode` (ValueEnum+serde), `Settings`, `DeviceParams`,
   `PreviewFrame`, `Shared` + helpers. `src/config.rs` — `Config`,
   `default_path()`, `load`, `save`, `merge(args, config) -> Resolved` with unit
   tests (roundtrip, precedence, missing file = defaults). Commit.
4. `src/pipeline.rs` — move the run loop; add outer restart loop, per-frame
   settings snapshot, bg reload on generation, preview publish, stats slot.
   `main.rs` keeps arg parsing/list/self-test and calls either
   `pipeline::run_blocking` (`--no-gui`) or spawns the thread + GUI. Build,
   `cargo test`, live `--no-gui` run verified on `/dev/video0` (regression:
   identical to M3). Commit.
5. `src/gui.rs` + `src/tray.rs` + main wiring (GUI default when a display is
   present). Build. Commit.
6. Verify (below) — GUI launched on Xwayland `:1`, screenshot with
   `import -window root`, config file inspected, tray registration checked via
   `busctl`/`qdbus` SNI watcher dump, `--no-gui` regression re-run. Commit.
7. README milestone-4 section + implementation findings appended here.

## Verify

- `cargo build --release` and `cargo test` pass with the env vars above.
- Config: first GUI/CLI run writes `~/.config/easyfilters/config.toml`; a
  second run with no flags picks the values up (assert via `--list`-style
  logging or a changed default mode in the live feed); `--config /tmp/x.toml`
  overrides the location; CLI flag beats config value.
- `--no-gui --mode blur` live on `/dev/video0` at 1280x720@30: read back with
  `scripts/debug_camera.py /dev/video0 --save` + ffmpeg; stats ≥ ~29 fps,
  0 drops — the M3 bar, i.e. the refactor changed nothing perf-wise.
- GUI: `./target/release/easyfilters` (Wayland or `DISPLAY=:1`) opens a
  window with a live moving preview; toggling mode/blur in the window changes
  the `/dev/video0` feed read back via debug_camera.py; editing the background
  path swaps the background live; closing the window or tray "Quit" shuts the
  pipeline down cleanly (stats log stops, streams off, exit 0).
- Tray: icon appears in the Plasma tray (SNI watcher list via
  `busctl --user` / `qdbus org.kde.StatusNotifierWatcher`); clicking
  enable/disable flips the feed between filtered and passthrough live.
- Screenshot evidence: `DISPLAY=:1 import -window root /tmp/gui.png` while the
  GUI runs, eyeballed.
- Regression: `--self-test`, `--list`, `--mode passthrough|overlay|mask`
  unchanged; headless fallback works with `DISPLAY= WAYLAND_DISPLAY=` unset.

## Risks / open questions

- **iced API drift:** 0.14's `application()` builder differs across minor
  versions; expect a compile-fix cycle on `Task`/`Subscription`/`window_size`
  signatures. Budget for it; the widget set used (row/column/image/toggler/
  pick_list/slider/text_input) is stable.
- **tiny-skia at 720p30:** a full-window RGBA re-upload per tick on CPU. If
  the preview stutters, downscale the preview to 640x360 (the loopback feed
  stays full-res) — preview is diagnostic, not the product.
- **ksni without a watcher:** on sessions with no StatusNotifierWatcher the
  tray thread must warn and exit quietly (app continues). KDE Plasma here has
  one, so live verification is possible.
- **Live stream restart:** changing camera/resolution re-opens devices; if the
  camera is busy (another app) the retry loop must not spin hot — 2 s backoff.
- **Config write races:** GUI and tray can both save; both serialize the same
  merged `Settings` under a mutex, so last-writer-wins is benign. Save writes
  to a temp file + rename to avoid a torn config on crash.
- **Pipeline thread + signal handler:** the handler only flips an atomic
  (async-signal-safe); all real shutdown happens in the threads.
- **Build time:** iced+tiny-skia+ksni(zbus) ≈ 350-450 crates; expect a
  several-minute first build on 12 cores, incremental after.
- **Debug builds stay ~3 fps** (tract unoptimized) — GUI perf checks must use
  `--release`.

## Implementation findings (2026-11-13, verified live)

- **iced feature correction:** the `thread-pool` executor backend has an empty
  `time` module — `iced::time::every` (needed for the preview subscription)
  only exists with the `smol` backend. Cargo.toml therefore uses
  `"smol"` instead of `"thread-pool"` (both pure Rust).
- **ksni needs libdbus at build time:** ksni 0.2 (pinned by tray-item 0.10)
  uses the C `dbus` crate (libdbus-sys), not zbus — `pkg-config dbus-1` must
  resolve. The host has no dbus dev package, so host builds use a shim
  `dbus-1.pc` (scratchpad) pointing at the nix store dbus dev/lib outputs;
  the flake just adds `dbus` to buildInputs (plus `dbus.lib` to the wrapped
  binary's LD_LIBRARY_PATH). Runtime libdbus-1.so.3 is a system lib here.
- **Threading works as designed:** pipeline thread + iced main loop + tray
  thread, coordinated only through `Shared`. Live-verified on Xwayland `:1`:
  327 preview frames in 12 s (~27 fps GUI refresh vs 29.7 fps pipeline) with
  `RUST_LOG=easyfilters=debug` (`new preview frame` log per frame), tray icon
  registered on the KDE StatusNotifierWatcher (Title "EasyFilters"), SIGINT
  → pipeline "stream stopped" → GUI exits → process gone.
- **Refactor perf-neutral:** release `--no-gui --mode blur` at 1280x720@30:
  **29.77 fps, 0 drops**, decode 2.3 / infer 6.7 / composite 5.6 / encode
  1.5 ms — the M3 bar (29.8 fps) unchanged with the pipeline on its own
  thread and per-frame settings snapshots.
- **Loopback readback caveat (environment, not a regression):** with Discord
  holding `/dev/video0` via PipeWire, direct V4L2 readers get EBUSY; readback
  then works through the PipeWire source node instead
  (`gst-launch-1.0 pipewiresrc path=149 ! image/jpeg ! jpegdec ! ...` — 5 valid
  1280x720 MJPG frames captured while the app streamed).
- **Config precedence verified live:** `--config` file with `mode = "mask"`
  resolves to mask; adding `--mode blur` on the CLI wins. `save_current`
  (GUI/tray changes) writes the merged settings atomically (tmp+rename);
  unit-tested roundtrip plus a `toggle_filtering` test that flips the flag
  and asserts the persisted `enabled = false`.
- **cargo test: 51 passed** — new coverage: config load/save/roundtrip/
  partial/malformed, resolve precedence, Mode clap↔serde name agreement,
  preview-slot gating, tray toggle, GUI `WxH` parsing.
- **Build process note:** long `cargo build --release` runs must go through the
  managed process namespace (tools.process.start) — nohup'd background
  cargo from a timed-out shell gets reaped mid-build.
- **`nix build .#default` passes** (EXIT=0, release profile 11m35s in the
  sandbox, deps vendored from Cargo.lock; wrapped `result/bin/easyfilters
  --help` runs). The flake's new `dbus`/`wayland`/`libxkbcommon` buildInputs
  cover libdbus-sys and winit. Only warning is the pre-existing
  function-item-cast in the signal handler.
- **Not verified here:** clicking the actual GUI controls (no input injection
  in this session; screenshots/D-Bus menu activation are blocked by the
  sandbox approval policy). The control handlers are thin wrappers over
  `Shared` + `save_current`, both unit-tested, and the preview/tray/signal
  paths were verified live as described above.
