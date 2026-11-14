# Milestone 8 — Record button: filtered video + PulseAudio default input into an .ogv

Goal: a **Record** control (GUI button, tray item, `--record` CLI flag) that
records the app's **video feed** — the post-filter frame the virtual camera
emits — together with the **PulseAudio default input device** into a single
`.ogv` file (Ogg container, Theora video + Vorbis audio).

Chosen approach (from the feasibility assessment): **option A — shell out to
`ffmpeg`**, feeding raw BGR frames on stdin and letting ffmpeg capture the
default pulse source itself. No new cargo dependencies; ffmpeg becomes a
runtime dependency of the nix package (already a dev tool in the devShell).

Repo state at planning time: branch `feat/vido_recording`, clean, at
`483042d` (milestone 7 complete and verified live: v4l2loopback + native
PipeWire camera node, iced GUI + ksni tray + TOML config, pipeline thread
driven by `Shared`; headless 1280x720@30 at ~29.8 fps, 0 drops in release).

## Environment findings (verified 2026-11-14)

- nixpkgs `ffmpeg` gating (checked against `nixpkgs-unstable`
  `pkgs/development/libraries/ffmpeg/generic.nix`): `withTheora ? withHeadlessDeps`,
  `withVorbis ? withHeadlessDeps`, `withPulse ? withSmallDeps && isLinux`.
  The devShell's current `pkgs.ffmpeg` therefore likely has **neither
  libtheora/libvorbis nor pulse**. Use **`pkgs.ffmpeg-full`** (all three) in
  both the devShell and the package wrapper, and assert at check time:
  `ffmpeg -hide_banner -encoders | grep -E 'libtheora|libvorbis'` and
  `ffmpeg -hide_banner -devices | grep pulse`.
- This machine's audio server is PipeWire 1.6.11 with the pulse compat
  server (pipewire-pulse), so ffmpeg's `-f pulse -i default` reaches "the
  PulseAudio default input" through the standard pulse protocol — no
  libpulse-binding crate needed.
- crates.io not needed: spawning ffmpeg is `std::process::Command`, the
  writer thread is `std::sync::mpsc`, SIGINT is `libc::kill` via
  `v4l2r::nix::libc` (already used in `main.rs` for the signal handler).
- CPU budget: M4/M5 run ~15–22 ms of the ~33 ms frame budget at 720p30, so
  the recorder must live off the hot path (writer thread + bounded channel,
  drop-frame policy). libtheora encode happens inside the ffmpeg process,
  i.e. off our thread entirely.

## Design decisions

### 1. What gets recorded

- **Video = the filtered output** (the same frame written to the v4l2loopback
  and the PipeWire node), tapped in `filter_frame` (`src/pipeline.rs`) right
  after composite + gaze, as a BGR `Mat` — `pix_fmt bgr24` matches the Mat
  layout exactly, zero-copy from `data_bytes()`. In passthrough/disabled
  mode `filter_frame` early-returns raw MJPG; when recording, decode it first
  (same decode the preview path already does) so the recording always shows
  what the virtual camera shows.
- Recording the **raw camera** instead is deliberately out of scope (a
  future `record_raw` option can tap one stage earlier).
- **Audio = PulseAudio default input** via ffmpeg `-f pulse -i default`
  (NULL device = server default source). If no pulse server is reachable,
  ffmpeg fails at start; we surface that in the GUI status and log, and do
  not kill the pipeline.

### 2. Recorder architecture — `src/recorder.rs` (new)

- `Recorder::start(w, h, fps, path) -> Result<Recorder>` spawns:
  ```
  ffmpeg -y -hide_banner -loglevel warning \
    -f rawvideo -pix_fmt bgr24 -s {w}x{h} -r {fps} \
    -use_wallclock_as_timestamps 1 -i pipe:0 \
    -f pulse -i default \
    -map 0:v -map 1:a -c:v libtheora -q:v 6 -c:a libvorbis -q:a 4 \
    -f ogg <path>
  ```
  with `stdin` piped, stderr piped to a per-recording log file
  (`<path>.ffmpeg.log`) so failures are diagnosable. ffmpeg binary resolution:
  `$EASYFILTERS_FFMPEG` > `ffmpeg` on `PATH` (the nix wrapper prefixes
  `PATH` with `ffmpeg-full`).
- Writer thread owns the stdin pipe; the capture loop hands it frames through
  a bounded `mpsc` channel (cap ~8 frames ≈ 22 MB at 720p). `try_send`; on
  full → drop and count (same drop-frame philosophy as the pipeline). The
  channel carries `(Vec<u8> bgr, Instant captured)`; the wall-clock timestamp
  is what keeps A/V in sync (see risk R1).
- `Recorder::stop()`: drop the sender, close stdin, send SIGINT
  (`libc::kill(pid, SIGINT)`) so ffmpeg writes a clean EOS and finalizes the
  ogg, `wait()` with a timeout, then SIGKILL if needed. Even a hard kill
  leaves a playable .ogv (Ogg is streamable; only the tail is lost).
- `Recorder` is `Send`; the `Child` handle + writer `JoinHandle` live in the
  struct; `Drop` = `stop()`.
- Output path: `$XDG_VIDEOS_DIR` (fallback `~/Videos`, created if missing),
  file `easyfilters-YYYYMMDD-HHMMSS.ogv`; a second start within the same
  second gets `-2`, `-3`, … Config: `record_dir: Option<PathBuf>` (CLI
  `--record-dir`), so the GUI/persisted config can point elsewhere.

### 3. Shared state + wiring (same patterns as milestones 4–7)

- `Shared` gains `recording: AtomicBool` and `record_generation: AtomicU64`
  (bumped on every toggle). The GUI/tray/CLI only flip these + persist
  config; the pipeline does the work.
- `capture_loop` watches `record_generation` each iteration (like
  `device_generation`): on a rising edge it builds the path and calls
  `Recorder::start(w, h, fps, path)`; on falling edge / stream restart /
  shutdown it stops the recorder. A device-settings change therefore
  restarts the stream and the recorder comes back with the new WxH
  automatically (rawvideo fixes dimensions at spawn).
- `filter_frame` gains a `rec: Option<&Recorder>` parameter; write the frame
  right after composite/gaze (and after the passthrough decode when
  `!enabled || mode == Passthrough`).
- `Stats` gains `record_frames: u64` / `record_drops: u64` so the GUI status
  line can show recording progress.
- Config: `record_dir: Option<PathBuf>` in `Config` + `save_current`.
  CLI: `--record-dir <path>`, and `--record <path>` to start recording at
  launch (headless-friendly; path may be a directory → timestamped file
  inside it).

### 4. GUI + tray

- GUI controls column: a `button("● Record")` / `button("■ Stop — <file>")`
  toggle (recording state re-synced on `Tick` like the other controls),
  plus the recording file + frame/drop counts in the existing status line.
- Tray: one more `add_menu_item("Toggle recording", …)` — same
  flip-and-persist pattern as `toggle_gaze` (`src/tray.rs`).

### 5. Packaging

- `flake.nix`: devShell `pkgs.ffmpeg` → `pkgs.ffmpeg-full`; package
  `buildInputs` unchanged, but `wrapProgram` gains
  `--prefix PATH : ${pkgs.ffmpeg-full}/bin`; the `checks.default` smoke test
  additionally asserts `ffmpeg-full` ships `libtheora`, `libvorbis` encoders
  and the `pulse` device.
- Non-nix installs: if `ffmpeg` is not on `PATH` (or lacks the encoders),
  record-start fails with a clear status message ("install ffmpeg with
  libtheora/libvorbis/pulse support, e.g. ffmpeg-full on nix") and a
  `warn!` — never fatal to the filtering pipeline.

## Implementation steps

1. `src/recorder.rs`: `Recorder` (spawn/ffmpeg-args builder as a pure
   function for unit tests, writer thread, bounded channel, stop/SIGINT,
   timestamped path generation incl. `-2` suffix, `record_dir` resolution).
   Unit tests: arg construction, path uniqueness, channel drop counting.
2. Verify the sync assumption early (before wiring the GUI): feed
   `ffmpeg -f rawvideo -use_wallclock_as_timestamps 1 -i pipe:0 …` synthetic
   frames with known wall-clock gaps from a scratch script, confirm
   `ffprobe` reports matching PTS. If rawdec ignores the flag, switch the
   writer thread to pad duplicate frames to wall-clock pace (count-based
   PTS then ≈ wall clock) — decide from this experiment, record the outcome
   in Implementation findings.
3. `src/settings.rs`: `Shared.recording` / `record_generation`, `Stats`
   fields; `src/config.rs`: `record_dir`; `src/main.rs`: `--record`,
   `--record-dir`, resolve wiring.
4. `src/pipeline.rs`: watch `record_generation` in `capture_loop`, own the
   `Option<Recorder>`, pass `&Recorder` into `filter_frame`, tap the frame.
5. `src/gui.rs`: Record button + status line; `src/tray.rs`: tray toggle.
6. `flake.nix`: `ffmpeg-full` in devShell + wrapper PATH + check assertions.
7. `cargo test --release`, `nix build`, `nix flake check`.

## Verify

1. `cargo test --release` green (new recorder unit tests included).
2. `nix build` + `nix flake check` green; check output asserts the
   libtheora/libvorbis encoders and pulse device in the wrapped ffmpeg.
3. Live (release build, real camera): toggle Record in the GUI for ~30 s,
   toggle Stop; then `ffprobe` the `.ogv`: exactly two streams
   (`theora` video at negotiated WxH@fps, `vorbis` audio), durations within
   ~0.5 s of each other and of the wall-clock recording time, all frames
   decodable (`ffmpeg -v error -i file -f null -` clean).
4. Pipeline health while recording: fps stays ~29.8, drops only counted in
   `Stats.record_drops` (loopback path never stalls); CPU headroom sane at
   720p30 q6.
5. Change resolution while recording (GUI Apply) → stream restarts,
   recorder restarts with the new WxH, old file finalized and playable.
6. Stop the app mid-recording (tray Quit / SIGINT) → recorder stopped,
   file playable up to the last flushed pages.
7. Record with no pulse server reachable (e.g. `PULSE_SERVER=/nonexistent`)
   → clear error in GUI status + log, pipeline unaffected, no crash.
8. `--record /tmp/x.ogv --no-gui` headless: file appears and grows; SIGINT
   stops cleanly.

## Risks / open questions

- **R1 — A/V sync:** rawvideo stdin is count-timestamped, but the pipeline
  drops frames; count-based PTS would drift against live audio over a long
  recording. Mitigation: `-use_wallclock_as_timestamps 1` (step 2 verifies),
  fallback = pad duplicates to wall-clock pace in the writer thread.
  Acceptance: ffprobe stream durations match wall clock within ~0.5 s.
- **R2 — Theora CPU cost:** libtheora q6 at 720p30 runs inside the ffmpeg
  process, so our hot path is safe, but total CPU rises; if the box struggles,
  lower to `-q:v 5` or downscale recorder input to 960x540 in the writer
  thread (opencv resize off the hot path). Measure in Verify step 4.
- **R3 — "default input" may be a monitor or the wrong mic on some setups.**
  The request is explicit (pulseaudio default input), so we record exactly
  that; a per-device picker is a follow-up.
- **R4 — ffmpeg availability/encoders on non-nix installs** — clear error
  message, never fatal (see Packaging).
- **R5 — disk usage:** 720p30 q6 + vorbis ≈ 1–2 GB/h. Log the file size on
  stop; no quota handling in MVP.
- **R6 — exclusive_caps / multi-reader:** irrelevant — we tap frames
  in-process, never re-open the loopback device.

## Follow-ups (out of scope here)

- Record the raw (unfiltered) camera instead of / in addition to the
  filtered feed.
- Audio device picker (any pulse source, not just default).
- In-process encoder (GStreamer-rs `appsrc ! theoraenc ! oggmux` +
  `pulsesrc`) if we ever need to drop the external ffmpeg dependency —
  heavier dep tree, exact PTS sync; not needed for MVP.
- Recording quality/resolution controls in the GUI.

## Implementation findings

(to be filled during implementation, verified live)
