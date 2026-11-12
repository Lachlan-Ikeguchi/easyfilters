# Milestone 2 — Segmentation: ONNX person mask in the preview feed

Goal (from `docs/idea.md`, milestone 2): integrate the ONNX segmentation model
(MediaPipe Selfie Segmentation, landscape 144x256) and show the person mask in
the preview. There is no GUI yet (that is milestone 4), so for this milestone
the "preview" is the virtual camera output itself: new `--mode` values make the
loopback feed carry a mask visualization (`overlay` / `mask`) that can be
watched in any camera app. The default `--mode passthrough` keeps the milestone
1 byte-copy behavior intact.

Repo state at planning time: branch `feat/milestone-2`, clean, at `30ebcb2`
(milestone 1 complete: MJPG passthrough verified live at ~29.8 fps 1280x720@30,
0 drops).

## Environment findings (verified on this machine, 2026-11-12)

- **Correction to the milestone-1 note "no browsers or Discord on this dev
  box": they are installed, as Flatpaks.** `flatpak list` shows
  `com.discordapp.Discord 1.0.169` and `org.mozilla.firefox 160.0`, plus camera
  preview apps usable for verification: `io.github.cosmic_utils.camera 1.11.0`,
  `org.gnome.Snapshot 51.0`, `org.videolan.VLC 3.0.24`, `thu.irl.cameractrls`,
  and `com.github.tchx84.Flatseal 2.4.1` (for granting sandbox device access).
  So app-compat checks now happen locally. Sandbox caveat: portal-based apps
  (Firefox) get cameras through PipeWire — the loopback is already a WirePlumber
  source (`wpctl status`: `149. EasyFilters Virtual Camera (V4L2)`), so it
  should just appear. Direct-V4L2 apps (Discord/Electron) may need
  `flatpak override --user --device=all com.discordapp.Discord` (or the
  Flatseal UI); verify in step 7.
- Loopback device live: `/dev/video0` = "EasyFilters Virtual Camera"
  (`exclusive_caps=0`, set up by `scripts/setup-virtual-camera.sh` during M1).
  Real camera: `/dev/video1` (uvcvideo, MJPG 1920x1080@30 … 320x180@30,
  YUYV max 640x480@30).
- Hardware: 12 cores, 15 GiB RAM — CPU inference of the 144x256 model at 30
  fps is comfortable.
- **OpenCV 4.13.0 is installed** (contrary to the Cargo.toml comment written
  during M1): `opencv4.pc` at `/usr/lib64/pkgconfig/opencv4.pc`, libs in
  `/usr/lib64`, headers at `/usr/include/opencv2/...` — note the **non-standard
  layout** (no `opencv4/` include dir), so the `opencv` crate build needs
  `OPENCV_INCLUDE_PATHS=/usr/include`. bindgen still needs `LIBCLANG_PATH` to
  the nix clang-lib (`/nix/store/...-clang-21.1.8-lib/lib`). Still missing:
  `libpipewire-0.3` and `gtk+-3.0` → keep the V4L2 capture path from M1 (do not
  enable the `pipewire` crate), keep GUI/tray (`iced`, `tray-item`) for M4.
- Network: huggingface.co reachable (HTTP 200). Model chosen:
  `onnx-community/mediapipe_selfie_segmentation_landscape`, file
  `onnx/model.onnx`, fp32, **462,338 bytes** (verified via `x-linked-size` on
  the resolve redirect), license Apache-2.0 (MediaPipe) — fine to vendor.
  Pinned URL:
  `https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape/resolve/main/onnx/model.onnx`
  (sha256 to be recorded in this file at step 2 when the file is vendored;
  the planning session could not fetch it, only HEAD it.)
- `tract-onnx` latest is 0.23.x (docs.rs) and is **pure Rust** — no system
  deps, which fits this nix-based box. Prefer it over `ort` (which needs a
  prebuilt onnxruntime). crates.io access works (M1 deps built fine).

## Design decisions

- **New dependencies:** `tract-onnx = "0.23"`, `ndarray` (tensor glue),
  `opencv = "0.99"` (MJPG decode, resize, JPEG encode; also the M3 compositing
  engine). `pipewire`/`iced`/`tray-item` stay commented out (M4). Fallback if
  the opencv crate build fails on this box (header layout / nix libclang):
  pure-Rust `jpeg-decoder` + `image` for M2 decode/encode only, opencv
  revisited in M3.
- **Capture stays V4L2** (`src/v4l2.rs` unchanged); the `pipewire` crate is
  blocked by the missing libpipewire-0.3 and V4L2 already works.
- **Output stays MJPG on the loopback** (proven in M1): decode the input MJPG
  frame → segment → render → re-encode JPEG (quality ~80) → write to the
  output queue. Only the bytes written change, not the output plumbing.
- **Model is vendored in the repo**: `assets/selfie_segmentation_landscape.onnx`
  (~452 KB) + `assets/LICENSE-mediapipe-selfie-segmentation.txt` (Apache-2.0).
  `--model <path>` / `EASYFILTERS_MODEL` override it; if the asset is missing
  the app downloads the pinned HF URL (via `curl`, present on the system) into
  `~/.cache/easyfilters/models/` and verifies the pinned sha256.
- **Model I/O is verified from the graph, not assumed.** Expected (MediaPipe
  landscape card): input float32, 144x256x3, normalized to [-1, 1] (ONNX
  export likely NCHW `[1,3,144,256]`); output sigmoid mask `[1,1,144,256]`,
  person ≈ 1, threshold 0.5. Step 3 dumps the actual input/output names and
  shapes with tract and records them in the findings below before wiring
  preprocessing.
- **CLI:** `--mode passthrough` (default; M1 byte-copy) | `overlay` (frame with
  the person kept and the background dimmed/tinted, mask edge visible) |
  `mask` (pure grayscale mask, person white). Plus `--self-test <image.jpg>`:
  decode one JPEG from disk, run the segmenter, write `mask.png` / `overlay.png`
  next to it and log mask coverage % — proves inference offline, no camera
  needed, and gives a regression artifact. Plus `--model <path>`.
- **Module layout:** `src/v4l2.rs` (unchanged), new `src/segmentation.rs`
  (`Segmenter`: load + verify model, preprocess frame → tensor, run, mask →
  8UC1), new `src/frame.rs` (imgdecode/imdecode, resize, overlay render,
  JPEG encode), `src/main.rs` wires `--mode` into the existing loop: the
  passthrough branch still memcpy's; the filter branches decode → segment →
  render → encode → qbuf. The 2 s stats log gains per-stage timings
  (`decode_ms`, `infer_ms`, `encode_ms`, `fps`, `drops`).
- **Single-threaded pipeline stays.** Inference at 144x256 is expected to be a
  few ms on 12 cores; only if measured `infer_ms` blows the frame budget do we
  move the segmenter to a worker thread with a one-frame queue. Drop-frame
  policy unchanged (drop rather than queue latency).
- **Temporal smoothing (EMA on the mask) is deferred to M3** per the roadmap —
  M2 output may flicker slightly at mask edges; acceptable for this milestone.

## Implementation steps

1. **Cargo.toml** — add `tract-onnx = "0.23"`, `ndarray = "0.16"`,
   `opencv = "0.99"` (keep pipewire/iced/tray commented). Build with
   `LIBCLANG_PATH=/nix/store/...-clang-21.1.8-lib/lib` and
   `OPENCV_INCLUDE_PATHS=/usr/include`; timebox the opencv bindgen build — on
   failure, swap in `jpeg-decoder` + `image` for M2 and note it here.
2. **Vendor the model** — download the pinned URL to
   `assets/selfie_segmentation_landscape.onnx`, add the Apache-2.0 license
   file, record `sha256sum` of the model in this plan.
3. **`src/segmentation.rs`** — load the ONNX via `tract-onnx`, print
   `model.inputs` / `model.outputs` (names, shapes, dtypes), build the
   `TypedRunnable`, implement preprocess (resize to the model's input size,
   BGR→RGB if required, normalize to [-1,1], layout per the dumped spec) and
   postprocess (sigmoid output → 8UC1 mask, threshold 0.5). Record the
   confirmed I/O spec in the findings below.
4. **`src/frame.rs`** — opencv helpers: `imgdecode` MJPG bytes → BGR `Mat`,
   `resize`, overlay render (`overlay`: background dimmed + slight tint,
   person untouched; `mask`: grayscale mask as BGR), `imencode` JPEG q80.
5. **`src/main.rs`** — add `--mode` / `--self-test` / `--model`; branch the
   pipeline loop as described; extend the stats log with per-stage ms.
6. **Build + self-test** — capture one real frame
   (`scripts/debug_camera.py /dev/video1 --frames 1 --save /tmp/frame.jpg`),
   run `easyfilters --self-test /tmp/frame.jpg`, sanity-check the logged mask
   coverage % and eyeball `mask.png` / `overlay.png`.
7. **Verify live** (see below).

## Verify

- `cargo build` succeeds (with the env vars above).
- `easyfilters --self-test /tmp/frame.jpg` → plausible mask coverage %;
  `mask.png` shows a person-shaped region.
- Run `easyfilters --mode overlay` and `--mode mask` in the background; read
  `/dev/video0` back with `v4l2-ctl --stream-mmap --stream-count=5`,
  `ffmpeg -f v4l2 -input_format mjpeg -i /dev/video0 -frames:v 5 -f null -`
  (valid MJPG out), and `scripts/debug_camera.py /dev/video0 --save`.
- Preview in the Flatpak apps: COSMIC Camera / Snapshot / VLC on the virtual
  camera (grant `/dev/video0` or `--device=all` via Flatseal if the sandbox
  hides it), Discord (flatpak) → Settings → Voice & Video → camera preview
  lists "EasyFilters Virtual Camera" with the overlay; Firefox (flatpak)
  camera picker via the PipeWire portal (already source `149` in `wpctl`).
  Note: apps enumerate cameras at startup — restart the app after starting
  easyfilters; document in README.
- `RUST_LOG=debug easyfilters --mode overlay` → stats every 2 s: expect
  ≥ ~25 fps at 1280x720@30 with ~0 drops and single-digit `infer_ms`.
- Regression: `--mode passthrough` still byte-copies (M1 behavior unchanged).

## Risks / open questions

- opencv crate vs the non-standard 4.13 header layout + nix libclang — mitigated
  by `OPENCV_INCLUDE_PATHS` / `LIBCLANG_PATH`; pure-Rust fallback for M2 if the
  bindgen build fails.
- Model I/O spec (NCHW vs HWC, normalization range) must come from the graph
  dump in step 3, not from assumption; recorded in findings when done.
- tract-onnx 0.23 is a large pure-Rust build (several minutes) — acceptable,
  cargo cache is warm from M1.
- Flatpak sandbox: Firefox via PipeWire portal already sees the source; Discord
  may need a device grant (Flatseal / `flatpak override --device=all`).
  Apps that enumerate cameras only at startup need a restart — README note.
- MJPG decode cost at 720p30: ~3–6 ms with opencv, more with pure-Rust
  `jpeg-decoder`; if fps suffers, drop to 960x540 or the YUYV input path.
- Mask flicker without temporal smoothing — deferred to M3 by design.
- Model sha256 gets pinned at step 2 (the planning session could only HEAD the
  URL, the download itself needs an approval).
