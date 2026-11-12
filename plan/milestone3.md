# Milestone 3 — Compositing: background blur + replace with temporal smoothing

Goal (from `docs/idea.md`, milestone 3): turn the milestone-2 segmentation
mask into real filters — **background blur** and **background replace**
(static image) with feathered mask edges, plus **temporal smoothing (EMA) of
the mask** to stop the edge flicker M2 deferred by design. There is still no
GUI (that is milestone 4), so the "preview" remains the virtual camera
output itself: new `--mode blur` / `--mode replace` values make the loopback
feed carry the filtered image, watchable in any camera app. Existing modes
(`passthrough` / `overlay` / `mask`) keep working unchanged.

Repo state at planning time: branch `feat/milestone-3`, clean, at `3efefb4`
(milestone 2 complete: `--mode overlay|mask` verified live on `/dev/video0`
at 1280x720@30, 29.8 fps, 0 drops, decode ~3 ms / infer ~7 ms / encode ~4 ms
in release, single-threaded).

## Environment findings (carried over from M1/M2, verified on this machine, 2026-11-12)

- `/dev/video0` = "EasyFilters Virtual Camera" loopback (`exclusive_caps=0`),
  `/dev/video1` = real uvcvideo camera (MJPG 1920x1080@30 … 320x180@30).
  Loopback also visible as PipeWire source node in `wpctl` and to Flatpak
  apps (Discord/Firefox carry `devices=all`; COSMIC Camera sees /dev/video0).
- opencv 0.102 + tract-onnx 0.23 already build here with
  `OPENCV_INCLUDE_PATHS=/usr/include`, `LIBCLANG_PATH=<nix clang-lib>/lib`,
  `LD_LIBRARY_PATH=<same>`, and the crate's `clang-runtime` feature.
  **Release build matters**: debug infer ~300 ms (~3 fps), release ~7 ms.
- Hardware: 12 cores, 15 GiB RAM. CPU budget at 720p30: M2 used ~14 ms of
  the ~33 ms frame budget, leaving headroom for compositing.
- Model I/O (confirmed in M2): input `pixel_values` F32 NCHW [1,3,144,256],
  output sigmoid mask [1,1,144,256], person ≈ 1, threshold 0.5.

## Design decisions

- **Plumbing untouched.** `src/v4l2.rs`, device discovery, the capture loop,
  the drop-frame policy and MJPG-on-the-loopback output all stay as they are.
  M3 only changes which bytes the filter branch writes.
- **New module `src/composite.rs`** holding everything M3 adds:
  - `MaskSmoother`: persistent f32 mask state at model resolution
    (144x256). Each frame: `state = α·new + (1−α)·state`, with α from
    `--mask-smoothing` (default 0.6). The composite thresholds the smoothed
    state at 0.5, but blur/replace use the soft (unthresholded) values for
    feathered edges. EMA on 36k pixels is sub-millisecond — negligible next
    to the 7 ms infer.
  - Feathering: small Gaussian blur (e.g. 5x5) on the mask at model res,
    then linear upscale to frame size — the upscale softens edges further
    for free. Add one full-res feather pass only if self-test artifacts
    show a hard edge.
  - `blur_background(frame, sigma)`: GaussianBlur the whole frame, then
    `out = mask·frame + (1−mask)·blurred` in f32 (mask replicated to 3
    channels). Blurring the full frame and masking afterward is much cheaper
    than blurring only background pixels. Sigma from `--blur-strength`
    (default ~15 at 720p; scale with resolution).
  - `replace_background(frame, bg)`: same alpha composite against a
    pre-loaded background Mat. `--background <path>` loads any image
    `imread` supports, resized once to the negotiated frame size. Without
    `--background`, generate a vertical gradient so the mode is testable
    out of the box (a bundled sample image in `assets/` is the alternative;
    decide at implementation time, prefer the generated gradient — zero
    binary assets).
- **CLI:** `--mode blur` | `--mode replace` added to the existing enum;
  `--blur-strength <sigma>` (default 15), `--background <path>`,
  `--mask-smoothing <α>` (default 0.6). Defaults for `--mode passthrough`
  unchanged.
- **Pipeline:** the filter arm in `main.rs` becomes
  decode → resize to model → segment → EMA update → feather+upscale mask →
  composite (blur or replace) → JPEG q80 → qbuf. The 2 s stats log gains
  `composite_ms` (everything after infer, before encode).
- **Self-test extended:** `--self-test frame.jpg` additionally writes
  `blur.png` and `replace.png` next to the input (using the default gradient
  background), so filtering is verifiable offline with no camera.
- **Single-threaded pipeline stays.** Target: still ~30 fps at 1280x720@30
  with 0 drops in release, i.e. compositing must fit in ~5 ms. If the
  full-res Gaussian blur blows the budget, downscale the background layer
  (blur at half res, upscale) — the person region is never blurred, so this
  is visually safe.
- **No new dependencies** — opencv already covers blur/blend/imread.

## Implementation steps

1. **`src/composite.rs`** (new) — `MaskSmoother` (EMA state + update +
   soft mask out), `feather_mask` (blur at model res), `blur_background`,
   `replace_background`, `default_background(w, h)` gradient, and the f32
   alpha-composite helper shared by both.
2. **`src/frame.rs`** — add `gaussian_blur(mat, sigma)`, `load_image(path)`,
   and any Mat conversion helpers the composite needs (8U↔32F, mask→3ch).
   Keep existing functions unchanged.
3. **`src/main.rs`** — add `Mode::Blur` / `Mode::Replace` and the three new
   flags; load the background (or generate the gradient) once after format
   negotiation; instantiate `MaskSmoother` before the loop; extend the
   filter arm with the composite step and `composite_ms` in stats; extend
   `self_test` to write `blur.png` / `replace.png`.
4. **Build** — `cargo build --release` with the env vars above; no new deps,
   so build time should stay ~incremental.
5. **Self-test** — capture a frame
   (`scripts/debug_camera.py /dev/video1 --frames 1 --save /tmp/frame.jpg`),
   run `easyfilters --self-test /tmp/frame.jpg`, eyeball `blur.png` /
   `replace.png`: person sharp, background blurred/replaced, soft edge,
   plausible mask coverage %.
6. **Verify live** (below).

## Verify

- `cargo build --release` succeeds with the env vars above.
- `--self-test /tmp/frame.jpg` → `blur.png` + `replace.png` look right
  (person sharp, background filtered, feathered edge, no black frames).
- Run `easyfilters --mode blur` and `--mode replace` in the background; read
  `/dev/video0` back with `scripts/debug_camera.py /dev/video0 --save` and
  `ffmpeg -f v4l2 -input_format mjpeg -i /dev/video0 -frames:v 5 -f null -`
  (valid MJPG out, visibly filtered frames).
- `RUST_LOG=debug` stats every 2 s: expect ≥ ~29 fps at 1280x720@30, 0
  drops, `composite_ms` single-digit, `infer_ms` ~7 ms — i.e. the M2
  performance bar still met with compositing on.
- Preview in a Flatpak camera app (COSMIC Camera / Discord → Voice & Video)
  on "EasyFilters Virtual Camera": blur / replace visible live. Restart the
  app after starting easyfilters (apps enumerate cameras at startup).
- Temporal smoothing: wave a hand / move in front of the camera in the live
  preview — mask edges should not flicker frame-to-frame (compare against
  `--mode overlay`, which uses the raw mask).
- Regression: `--mode passthrough` still byte-copies; `--mode overlay|mask`
  unchanged; `--list` unchanged.

## Risks / open questions

- **Smoothing vs. latency:** high α smooths more but the mask lags when you
  move. Default 0.6 is a guess — tune from the live preview, keep it a flag
  so M4's GUI can expose it without a rebuild.
- **No-person frames:** with EMA, a briefly empty mask fades out instead of
  snapping to a full background — usually desirable; check in the artifacts.
- **Blur strength default:** "looks good in a meeting" can't be asserted in
  a test; pick sigma ~15 at 720p and verify visually, scale with resolution.
- **Background aspect ratio:** resize (stretch) to frame size once at load
  vs. cover-crop — decide at implementation; stretch is simplest and fine
  for the M3 preview.
- **Composite cost at 1080p:** 720p30 is the bar; 1080p30 remains a stretch
  goal (M2 notes), compositing makes it harder — half-res background layer
  is the mitigation if needed.
- Debug builds stay ~3 fps (tract unoptimized); always measure with
  `--release`.
- Flatpak restart-after-start caveat applies as in M1/M2 — README note
  already covers it.
