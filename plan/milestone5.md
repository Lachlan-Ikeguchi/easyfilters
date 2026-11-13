# Milestone 5 — Gaze correction ("eye contact"): subtle, capped, landmark-based pupil shift

Goal (from `docs/idea.md`, originally deferred to "later"): bring back
**eye-contact / gaze correction** as an **optional, capped** effect — a
subtle landmark-based pupil shift that makes the user appear to look at the
camera in the virtual feed. Production-quality gaze redirection (NVIDIA
Maxine) needs an RTX GPU and is proprietary; what we build here is the
CPU-feasible version the idea doc always allowed for: run a face-landmark
model, estimate where each iris sits in its eye opening, and shift the iris
texture toward "looking at the webcam" by a small, hard-capped amount.

Repo state at planning time: branch `master`, clean, at `c899179`
(milestone 4 complete and verified live: iced GUI + ksni tray + TOML config,
pipeline thread driven by `Shared`; `--no-gui` headless at 1280x720@30,
29.8 fps, 0 drops, decode ~2.5 / infer ~6 / composite ~4.5 / encode ~1.7 ms
in release, single-threaded).

## Environment findings (carried over from M4, verified on this machine, 2026-11-13)

- Session: KDE Plasma Wayland, D-Bus session bus with a StatusNotifierWatcher;
  `/dev/video0` = "EasyFilters Virtual Camera" loopback, `/dev/video1` = real
  uvcvideo camera, `/dev/video2` = metadata. 12 cores, 15 GiB RAM.
- tract-onnx 0.23 + opencv 0.102 build green via the nix flake; release build
  matters (debug infer ~300 ms vs ~6-7 ms release for the segmentation model —
  the face models will be the same, so all perf verification uses `--release`).
- crates.io reachable; MediaPipe models are Apache-2.0 (same license as the
  bundled selfie-segmentation model), so bundling ONNX exports in `assets/`
  with the license file is fine.
- CPU budget at 720p30: M4 uses ~15 ms of the ~33 ms frame budget. Gaze adds
  an estimated ~2-3 ms/frame amortized — fits.

## Design decisions

- **Models** (both MediaPipe, Apache-2.0, bundled in `assets/` like the
  segmentation model, with the license note in `assets/`):
  - **Face detection**: BlazeFace short-range (input 128x128 RGB normalized
    to [-1,1], outputs anchors+scores; ~0.2-1 ms on CPU). Runs every frame on
    a 256x256 square downscale of the input to find the largest face box.
  - **Face mesh with iris refinement**: MediaPipe Face Mesh 478-landmark
    variant (input 192x192 RGB normalized to [-1,1], output [1,478,3] x/y
    normalized + z; ~3-5 ms on CPU). Runs on the (expanded, squared) face crop
    every 3rd frame; single face only (largest box) — MVP is one user.
  - Sourcing: official MediaPipe `.tflite` converted to ONNX, or a known-good
    Apache-2.0 ONNX export; input/output specs verified with the
    `examples/dump_model.rs` pattern before writing any inference code.
  - Overrides: `--face-model <path>` / `EASYFILTERS_FACE_MODEL` for the mesh
    (detector stays bundled), mirroring `--model` for segmentation.
- **Geometry** (pure functions in `src/gaze.rs`, unit-tested on synthetic
  landmarks — no model needed):
  - Landmark indices (478-landmark model): left eye corners 33 (outer) /
    133 (inner), top 159 / bottom 145, iris center 468 with rim 469-472; right
    eye corners 362 (inner) / 263 (outer), top 386 / bottom 374, iris center
    473 with rim 474-477.
  - Per eye: eye center = midpoint of the corners, eye size = corner distance
    (x) and top-bottom distance (y), iris center + radius from 468/473 and rim.
  - Target shift = move the iris toward the center of the eye opening, plus a
    small fixed bias toward the image center (where the webcam sits), so the
    gaze appears aimed at the camera lens. Normalized offset → pixels at full
    resolution, scaled by `gaze_strength`, then **hard-capped**:
    `max_shift = min(0.25 * eye_width_px, 3.5 px * h/720)` per axis.
  - Gates (skip the eye / the frame, no-op gracefully): face roll beyond
    ~25°, eye aspect (top-bottom / corner distance) below ~0.2 (blink), face
    box smaller than ~40 px at 720p, iris offset jitter above a variance
    threshold over the smoothing window (glasses/occlusion heuristic).
  - Temporal smoothing: EMA on the per-eye shift vector (same pattern as
    `composite::MaskSmoother`), mesh every 3rd frame, plus a 0.5 px deadband —
    landmark jitter must not make the pupils swim.
- **Rendering**: per eye, take a bounding box (corners ± iris radius + cap +
  margin) and `cv::remap` it so pixels inside the iris disc sample from
  `pos - shift` (the iris texture moves by `shift`), identity outside, with a
  feathered annulus between iris radius and radius+blend for a soft edge.
  Copy the box back into the frame through the person mask (eyes are inside
  the face, but this stops a spurious face in the background image from being
  "corrected"). Cost is two small remaps per frame, < 1 ms at 720p.
- **Where it hooks in**: new `src/gaze.rs` (`Detector` + `FaceMesh` tract
  wrappers like `Segmenter`, pure geometry, `GazeCorrector` holding models +
  smoothed state). In `pipeline::filter_frame`, after the composite stage and
  before encode/preview, for modes `overlay` / `blur` / `replace` only: when
  `settings.gaze_correction` is on, lazily load the models (first filtered
  frame, like the segmenter), run detection every frame + mesh every 3rd on
  the gaze input, then apply the smoothed shift to `out_img`. `mask` mode
  outputs a mask (no face to correct) and passthrough/disabled byte-copies —
  gaze simply does not apply there (it is part of filtering). New
  `Stats::gaze_ms` alongside the other stage timings.
- **Settings / config / CLI** (same precedence as M4: CLI > config >
  default):
  - `Settings.gaze_correction: bool` — **default false (opt-in)**: this is the
    one effect that can look uncanny, so it ships off and is one toggle away.
  - `Settings.gaze_strength: f32`, 0..=1, default 0.5 — scales the shift; the
    absolute pixel cap above still applies on top.
  - Config: `gaze_correction = true`, `gaze_strength = 0.5` (optional fields,
    `#[serde(default)]`). CLI: `--gaze` / `--no-gaze`, `--gaze-strength <0..1>`.
  - DeviceParams: `face_model: Option<PathBuf>` + `EASYFILTERS_FACE_MODEL`.
- **GUI**: a "Gaze correction" `toggler` + a strength `slider(0.0..=1.0)
  .step(0.01)` — remember iced sliders default to `step = 1` (the c899179
  bug); every iced float slider needs an explicit `.step(...)`. **Tray**:
  third menu item "Toggle gaze correction" (flips the flag + persists).
- **Self-test**: `--self-test frame.jpg` additionally runs the gaze stage on
  the test image, writes `gaze.png` next to the other artifacts, and prints
  the detected per-eye shift in px.
- **No new crates**: tract-onnx + opencv already cover inference and remap.
  `nix build` stays pure-Rust-crates-only.

## Implementation steps

1. `plan/milestone5.md` (this file) + `docs/idea.md` update (gaze moves from
   "out of scope" to milestone 5; polish becomes milestone 6) — commit as docs.
2. Acquire + bundle the ONNX models in `assets/` (+ license note), verify
   input/output specs via a dump example, wire `--face-model` /
   `EASYFILTERS_FACE_MODEL` resolution. Commit.
3. `src/gaze.rs` — `Detector`/`FaceMesh` wrappers, pure geometry
   (eye boxes, iris offset, capped target shift, roll/blink gates), EMA
   smoothing, remap-based `apply`. Unit tests on synthetic landmarks (shift
   zero when centered, direction toward center, cap respected, gates) plus a
   real-model shape/range test. Commit.
4. `settings.rs` / `config.rs` / `main.rs` — new fields, CLI flags, `resolve`
   precedence, serde roundtrip + precedence tests. Commit.
5. `pipeline.rs` — gaze stage in `filter_frame` (lazy model load, mesh every
   3rd frame, apply after composite, `gaze_ms` stat, preview published after
   correction); `cargo test`, live `--no-gui --gaze` run on `/dev/video0`.
   Commit.
6. `gui.rs` + `tray.rs` — toggler + slider (explicit `.step(0.01)`) + tray
   toggle item. Build, commit.
7. Verify (below) — self-test image, live feed with head movement (screenshot
   evidence), GUI/tray/config roundtrip, perf bar, M4 regression with gaze
   off. README milestone-5 section + findings appended here. Commit.

## Verify

- `cargo test` and `nix build` / `nix flake check` green; release binary.
- `--self-test` on a face photo: writes `gaze.png`, prints per-eye shift px;
  pupils visibly nearer the eye centers in `gaze.png` than in the input.
- Live: `./target/release/easyfilters --no-gui --gaze` at 1280x720@30 on
  `/dev/video0`; capture the feed with `scripts/debug_camera.py`; move the
  head side to side — in the output the pupils stay near the eye centers
  (screenshot evidence via `import -window root` on Xwayland `:1`).
  Stats: `gaze_ms` present, fps ≥ ~25 at 720p30, 0 drops.
- GUI: the toggler/slider change the live feed and persist to
  `~/.config/easyfilters/config.toml`; tray "Toggle gaze correction" flips it;
  a second launch resumes with the saved values (CLI flag still wins).
- Regression: with gaze off (default), behavior and fps are identical to M4
  (~29.8 fps); `--mode passthrough|overlay|mask`, `--list`, `--self-test`
  unchanged.

## Risks / open questions

- **Model sourcing**: need ONNX exports of Face Mesh *with iris landmarks*
  (478 pts) and BlazeFace short-range, both Apache-2.0. If no trustworthy
  478-landmark ONNX export is found, fall back to the 468-landmark model and
  estimate the iris center from the eye-contour landmarks (still capped) —
  decide after inspecting the exports in step 2.
- **Uncanny valley**: default off, hard pixel cap, roll/blink/jitter gates —
  the effect must be subtle or it is worse than none.
- **Jitter**: mesh at reduced rate + EMA + deadband; if pupils still swim,
  raise the smoothing alpha and mesh interval.
- **Perf**: amortized target ≤ 3 ms/frame at 720p; if `gaze_ms` exceeds ~5 ms,
  drop the mesh to every 5th frame and skip detection for a few frames after
  losing the face.
- **Multiple faces / occlusion / glasses**: single largest face only;
  variance gate skips unstable landmarks. No multi-face correction in M5.
- **Debug builds** are ~40x slower on tract models — all live verification in
  `--release`, same as M2-M4.

## Implementation findings (all verified live, 2026-11-13, release build)

- **Model sourcing**: HF community ONNX exports (Apache-2.0, format-only
  conversions of Google's models), bundled in `assets/` with provenance +
  SHA-256 in `assets/LICENSE-mediapipe-face-models.txt`:
  - `face_landmarks_478.onnx` (naklitechie/face-landmarks-onnx, SHA-256
    matches the publisher's): input `input_12` [1,256,256,3] F32 **NHWC RGB
    in [0,1]** (verified: presence sigmoid ~1.0 on a live face crop with
    [0,1], ~0.3 with [-1,1]); outputs `Identity` [1,1,1,1434] = 478×(x,y,z)
    **in crop pixels**, `Identity_1` presence logit, `Identity_2`. Symbolic
    batch bound to 1 via the `Segmenter::load` set_symbols pattern.
  - `blaze_face_short_range.onnx` (garavv/blazeface-onnx): inputs `image`
    [1,3,128,128] F32 NCHW RGB **[-1,1]** (fires at conf 0.5 on live frames;
    [0,255] finds nothing), `conf_threshold` [1] F32, `max_detections` [1]
    I64, `iou_threshold` [1] F32; output `selectedBoxes` [1,N,16] with rows
    **[y1, x1, y2, x2] corner-format** + 6 keypoints, normalized per axis —
    center-format interpretation puts the box off the actual face (verified
    against the mesh presence head on live frames).
  - Face-crop rule: square side = max(box w,h) × 1.25 centered on the box
    (1.1–1.3 all give mesh presence ≈ 1.0; 1.6+ drops it) — tighter/looser
    crops verified live.
- **opencv crate 0.102**: `Mat::roi` returns `BoxedRef<Mat>` which does NOT
  implement `Deref` — pass ROIs to functions taking `&impl MatTraitConst`
  (opencv ops) directly; only `&Mat`-typed helpers need a different path.
  2-D `at_mut` is not generated; fill f32 maps via `data_bytes_mut` offsets.
- **Self-test on a live 1280x720 frame** (user in profile): gaze shift left
  (2.82, 0.38) px, right (3.50, 0.83) px — both at the 3.5 px 720p cap,
  direction toward image center; eye crop before/after shows the pupil
  visibly nearer the eye center in `gaze.png`.
- **Live headless `--gaze` at 1280x720@30 on /dev/video0**: 29.8 fps, 0
  drops, gaze_ms ≈ 4.2–4.5 (detector every frame + mesh every 3rd + remap;
  under the ~5 ms risk threshold, so mesh stays at every 3rd frame), total
  ~22 ms of the 33 ms budget. With gaze off (default): gaze_ms = 0.0, fps
  29.7–29.8 — identical to M4 (regression clean).
- **Config/CLI precedence live**: `gaze_correction = true` in the TOML
  engages gaze (models load once, gaze_ms > 0); `--no-gaze` on the CLI
  overrides it (gaze_ms = 0.0, models never load). `--gaze-strength` parser
  rejects out-of-range values.
- **GUI smoke test on Xwayland :1**: iced window created, "tray icon
  registered" (now 3 menu items), pipeline at 29.7–29.9 fps with preview on;
  gaze toggler + slider follow the same apply_and_save path as the M4
  controls (unit-tested via `toggle_gaze`).
- `cargo test --release`: 71 passed, 0 failed (17 new gaze tests: geometry,
  gates, smoother, remap, real-model shapes). `nix build` + `nix flake
  check` green; the flake package now ships all three ONNX models and sets
  `EASYFILTERS_MODEL` / `EASYFILTERS_FACE_MODEL` / `EASYFILTERS_FACE_DETECTOR`.
- Known limits: single largest face only; profile/occluded eyes rely on the
  jitter + presence gates; the 128x128 detector input squashes aspect (box
  mapping back per-axis handles it).
