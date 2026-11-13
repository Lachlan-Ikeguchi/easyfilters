//! Gaze correction ("eye contact", milestone 5): an optional, capped,
//! landmark-based pupil shift. Two MediaPipe models (Apache-2.0, bundled in
//! `assets/`, see assets/LICENSE-mediapipe-face-models.txt):
//!
//! - `blaze_face_short_range.onnx` — BlazeFace short-range detector.
//!   Verified I/O (examples/probe_face.rs on live camera frames):
//!     inputs  "image" [1,3,128,128] F32 NCHW RGB normalized to [-1,1],
//!             "conf_threshold" [1] F32, "max_detections" [1] I64,
//!             "iou_threshold" [1] F32
//!     output  "selectedBoxes" [1,N,16] F32, rows = [y1, x1, y2, x2,
//!             6 x (x,y) keypoints], normalized per axis (corner format —
//!             verified: center-format interpretation puts the box off the
//!             actual face).
//!   Runs every frame on a 128x128 resize of the input; largest box wins.
//! - `face_landmarks_478.onnx` — MediaPipe Face Mesh V2 (478 landmarks incl.
//!   iris) from face_landmarker.task, tf2onnx opset 17.
//!     input  "input_12" [1,256,256,3] F32 NHWC RGB in [0,1]
//!     outputs "Identity" [1,1,1,1434] = 478 x (x,y,z) in CROP PIXELS,
//!             "Identity_1" [1,1,1,1] face-presence logit (sigmoid it),
//!             "Identity_2" [1,1]
//!   Runs on the (expanded, squared) face crop every MESH_EVERY frames.
//!
//! Geometry (pure functions below, unit-tested on synthetic landmarks):
//! per eye, move the iris toward the eye-opening center plus a small fixed
//! bias toward the image center (where the webcam sits), scaled by
//! `gaze_strength` and hard-capped at `min(0.25 * eye_width, 3.5px * h/720)`
//! per axis. Gates skip the eye/frame on roll > 25°, blink (eye aspect
//! < 0.2), tiny faces, low mesh presence, or iris jitter above a variance
//! threshold. EMA + 0.5 px deadband keep the pupils from swimming.
//!
//! Rendering: per eye a `cv::remap` of a small bounding box samples from
//! `pos - shift` inside the iris disc (feathered annulus outside), and the
//! box is copied back through the person mask. < 1 ms/frame at 720p.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use opencv::core::{Mat, Rect, Scalar, CV_32FC1};
use opencv::prelude::*;
use opencv::{core, imgproc};
use tract_onnx::prelude::*;

use crate::frame;

/// BlazeFace input size (square).
pub const DETECT_SIZE: usize = 128;
/// Face-mesh input size (square crop).
pub const MESH_SIZE: usize = 256;
pub const NUM_LANDMARKS: usize = 478;

// MediaPipe 478-landmark indices for the eyes and irises.
pub const LM_LEFT_OUTER: usize = 33;
pub const LM_LEFT_INNER: usize = 133;
pub const LM_LEFT_TOP: usize = 159;
pub const LM_LEFT_BOTTOM: usize = 145;
pub const LM_LEFT_IRIS: usize = 468; // rim 469..=472
pub const LM_RIGHT_INNER: usize = 362;
pub const LM_RIGHT_OUTER: usize = 263;
pub const LM_RIGHT_TOP: usize = 386;
pub const LM_RIGHT_BOTTOM: usize = 374;
pub const LM_RIGHT_IRIS: usize = 473; // rim 474..=477

/// Gates — when tripped, skip the eye (or the whole frame) gracefully.
pub const MAX_ROLL_DEG: f32 = 25.0;
pub const MIN_EYE_ASPECT: f32 = 0.2; // top-bottom / corner distance; below = blink
pub const MIN_FACE_PX_AT_720: f32 = 40.0; // min face box side, scaled by h/720
pub const MIN_PRESENCE: f32 = 0.5; // sigmoid of the mesh presence logit
pub const JITTER_VAR_MAX: f32 = 4.0; // px^2 variance of raw shift over the window
pub const DEADBAND_PX: f32 = 0.5;
pub const MESH_EVERY: u64 = 3;
pub const SMOOTH_ALPHA: f32 = 0.5;
const JITTER_WINDOW: usize = 5;
/// Face crop square side = max(box w, h) * CROP_EXPAND, centered on the box
/// (verified live: corner-box * 1.1..1.3 gives mesh presence ~1.0).
const CROP_EXPAND: f32 = 1.25;
const DETECT_CONF: f32 = 0.5;
const DETECT_MAX: i64 = 10;
const DETECT_IOU: f32 = 0.3;

// ---------------------------------------------------------------- geometry

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EyeGeom {
    /// Eye-opening center, frame px.
    pub center: (f32, f32),
    /// Corner-to-corner distance (eye width), frame px.
    pub width: f32,
    /// Top-to-bottom distance (eye height), frame px.
    pub height: f32,
    /// Iris center, frame px.
    pub iris: (f32, f32),
    /// Mean iris-rim radius, frame px.
    pub iris_radius: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EyeSide {
    Left,
    Right,
}

/// Per-eye geometry from 478 landmarks (frame px). None if degenerate.
pub fn eye_geom(lm: &[[f32; 3]], side: EyeSide) -> Option<EyeGeom> {
    let (outer, inner, top, bottom, iris_c, rim) = match side {
        EyeSide::Left => (
            LM_LEFT_OUTER,
            LM_LEFT_INNER,
            LM_LEFT_TOP,
            LM_LEFT_BOTTOM,
            LM_LEFT_IRIS,
            469..=472,
        ),
        EyeSide::Right => (
            LM_RIGHT_OUTER,
            LM_RIGHT_INNER,
            LM_RIGHT_TOP,
            LM_RIGHT_BOTTOM,
            LM_RIGHT_IRIS,
            474..=477,
        ),
    };
    let o = lm[outer];
    let i = lm[inner];
    let t = lm[top];
    let b = lm[bottom];
    let c = lm[iris_c];
    let width = ((o[0] - i[0]).powi(2) + (o[1] - i[1]).powi(2)).sqrt();
    let height = ((t[0] - b[0]).powi(2) + (t[1] - b[1]).powi(2)).sqrt();
    if width < 1e-3 || height < 1e-3 {
        return None;
    }
    let center = ((o[0] + i[0]) / 2.0, (o[1] + i[1]) / 2.0);
    let iris = (c[0], c[1]);
    let iris_radius = rim
        .map(|r| {
            let p = lm[r];
            ((p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sqrt()
        })
        .sum::<f32>()
        / 4.0;
    Some(EyeGeom {
        center,
        width,
        height,
        iris,
        iris_radius,
    })
}

/// Eye aspect ratio = height / width; below MIN_EYE_ASPECT = blink.
pub fn eye_aspect(e: &EyeGeom) -> f32 {
    e.height / e.width
}

/// Face roll in degrees from the line between the two eye centers.
pub fn face_roll_deg(lm: &[[f32; 3]]) -> f32 {
    let (Some(l), Some(r)) = (eye_geom(lm, EyeSide::Left), eye_geom(lm, EyeSide::Right)) else {
        return 0.0;
    };
    (r.center.1 - l.center.1)
        .atan2(r.center.0 - l.center.0)
        .to_degrees()
        .abs()
}

/// Hard per-axis cap of the pupil shift, frame px.
pub fn max_shift_px(eye_width_px: f32, frame_h: u32) -> f32 {
    (0.25 * eye_width_px).min(3.5 * frame_h as f32 / 720.0)
}

/// Desired iris shift (frame px): move the iris toward the eye-opening
/// center plus a small bias toward the image center (webcam position),
/// scaled by `strength` and hard-capped per axis.
pub fn target_shift(eye: &EyeGeom, img_center: (f32, f32), strength: f32, frame_h: u32) -> (f32, f32) {
    let cap = max_shift_px(eye.width, frame_h);
    let mut dx = eye.center.0 - eye.iris.0;
    let mut dy = eye.center.1 - eye.iris.1;
    let (bx, by) = (img_center.0 - eye.center.0, img_center.1 - eye.center.1);
    let bl = (bx * bx + by * by).sqrt().max(1e-3);
    let bias = 0.3 * eye.width;
    dx += bx / bl * bias;
    dy += by / bl * bias;
    let s = strength.clamp(0.0, 1.0);
    ((dx * s).clamp(-cap, cap), (dy * s).clamp(-cap, cap))
}

/// EMA smoother over the per-eye shift vector with a deadband, plus a
/// sliding window of raw measurements for the jitter gate.
pub struct ShiftSmoother {
    alpha: f32,
    state: Option<(f32, f32)>,
    raw_window: Vec<(f32, f32)>,
}

impl ShiftSmoother {
    pub fn new(alpha: f32) -> Self {
        Self {
            alpha: alpha.clamp(0.0, 1.0),
            state: None,
            raw_window: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.state = None;
        self.raw_window.clear();
    }

    /// Record a raw measurement; returns false when the jitter gate trips
    /// (variance over the window above JITTER_VAR_MAX) and the caller should
    /// hold the current state instead of updating.
    pub fn observe(&mut self, raw: (f32, f32)) -> bool {
        self.raw_window.push(raw);
        if self.raw_window.len() > JITTER_WINDOW {
            self.raw_window.remove(0);
        }
        self.jitter_var() <= JITTER_VAR_MAX
    }

    /// Max per-axis variance of the raw window.
    pub fn jitter_var(&self) -> f32 {
        if self.raw_window.len() < 2 {
            return 0.0;
        }
        let n = self.raw_window.len() as f32;
        let mx = self.raw_window.iter().map(|r| r.0).sum::<f32>() / n;
        let my = self.raw_window.iter().map(|r| r.1).sum::<f32>() / n;
        let vx = self.raw_window.iter().map(|r| (r.0 - mx).powi(2)).sum::<f32>() / n;
        let vy = self.raw_window.iter().map(|r| (r.1 - my).powi(2)).sum::<f32>() / n;
        vx.max(vy)
    }

    /// EMA update with a DEADBAND_PX deadband; returns the new state.
    pub fn update(&mut self, raw: (f32, f32)) -> (f32, f32) {
        let next = match self.state {
            None => raw,
            Some(cur) => {
                let d = ((raw.0 - cur.0).powi(2) + (raw.1 - cur.1).powi(2)).sqrt();
                if d < DEADBAND_PX {
                    cur
                } else {
                    (
                        self.alpha * raw.0 + (1.0 - self.alpha) * cur.0,
                        self.alpha * raw.1 + (1.0 - self.alpha) * cur.1,
                    )
                }
            }
        };
        self.state = Some(next);
        next
    }
}

// ---------------------------------------------------------------- models

fn load_runnable(path: &Path) -> Result<Arc<TypedRunnableModel>> {
    let model = tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("load ONNX model {}", path.display()))?;
    let typed = model.into_typed()?;
    // Bind symbolic dims (batch) to 1 — same pattern as Segmenter::load.
    let subs: HashMap<Symbol, TDim> = typed
        .symbols
        .all_symbols()
        .into_iter()
        .map(|s| (s, TDim::from(1usize)))
        .collect();
    let typed = typed.set_symbols(&subs)?;
    let typed = typed.into_optimized()?;
    Ok(typed.into_runnable()?)
}

/// BlazeFace short-range face detector.
pub struct Detector {
    model: Arc<TypedRunnableModel>,
}

impl Detector {
    pub fn load(path: &Path) -> Result<Self> {
        let model = load_runnable(path)?;
        let fact = model.input_fact(0)?;
        let dims = fact.shape.dims();
        ensure!(
            fact.datum_type == DatumType::F32
                && dims.len() == 4
                && dims[0] == TDim::from(1usize)
                && dims[1] == TDim::from(3usize)
                && dims[2] == TDim::from(DETECT_SIZE)
                && dims[3] == TDim::from(DETECT_SIZE),
            "unexpected detector input spec: {:?} {:?}",
            fact.datum_type,
            dims
        );
        Ok(Self { model })
    }

    /// `rgb`: tightly packed RGB8 at DETECT_SIZE x DETECT_SIZE. Returns the
    /// largest face as [y1, x1, y2, x2] normalized per axis, or None.
    pub fn largest_face(&self, rgb: &[u8]) -> Result<Option<[f32; 4]>> {
        ensure!(
            rgb.len() == DETECT_SIZE * DETECT_SIZE * 3,
            "bad rgb len {} (want {})",
            rgb.len(),
            DETECT_SIZE * DETECT_SIZE * 3
        );
        // HWC RGB u8 -> CHW f32 normalized to [-1, 1].
        let mut chw = vec![0f32; 3 * DETECT_SIZE * DETECT_SIZE];
        for p in 0..DETECT_SIZE * DETECT_SIZE {
            for c in 0..3 {
                chw[c * DETECT_SIZE * DETECT_SIZE + p] =
                    (rgb[p * 3 + c] as f32 / 255.0 - 0.5) * 2.0;
            }
        }
        let image = Tensor::from(
            tract_ndarray::Array4::from_shape_vec((1, 3, DETECT_SIZE, DETECT_SIZE), chw)
                .context("build detector input")?,
        );
        let conf = Tensor::from(tract_ndarray::Array1::from_elem([1], DETECT_CONF));
        let max_det = Tensor::from(tract_ndarray::Array1::from_elem([1], DETECT_MAX));
        let iou = Tensor::from(tract_ndarray::Array1::from_elem([1], DETECT_IOU));
        let outs = self
            .model
            .run(tvec!(
                image.into_tvalue(),
                conf.into_tvalue(),
                max_det.into_tvalue(),
                iou.into_tvalue()
            ))?;
        ensure!(outs.len() == 1, "expected 1 output, got {}", outs.len());
        let view = outs[0].to_plain_array_view::<f32>()?; // [1, N, 16]
        let mut best: Option<[f32; 4]> = None;
        let mut best_area = 0.0f32;
        for row in view.axis_iter(tract_ndarray::Axis(1)) {
            let r: Vec<f32> = row.iter().copied().collect();
            let (y1, x1, y2, x2) = (r[0], r[1], r[2], r[3]);
            let w = x2 - x1;
            let h = y2 - y1;
            if w > 0.0 && h > 0.0 && w * h > best_area {
                best_area = w * h;
                best = Some([y1, x1, y2, x2]);
            }
        }
        Ok(best)
    }
}

/// MediaPipe Face Mesh V2 (478 landmarks incl. iris).
pub struct FaceMesh {
    model: Arc<TypedRunnableModel>,
}

impl FaceMesh {
    pub fn load(path: &Path) -> Result<Self> {
        let model = load_runnable(path)?;
        let fact = model.input_fact(0)?;
        let dims = fact.shape.dims();
        ensure!(
            fact.datum_type == DatumType::F32
                && dims.len() == 4
                && dims[0] == TDim::from(1usize)
                && dims[1] == TDim::from(MESH_SIZE)
                && dims[2] == TDim::from(MESH_SIZE)
                && dims[3] == TDim::from(3usize),
            "unexpected mesh input spec: {:?} {:?}",
            fact.datum_type,
            dims
        );
        Ok(Self { model })
    }

    /// `rgb`: tightly packed RGB8 at MESH_SIZE x MESH_SIZE. Returns the 478
    /// landmarks ([x, y, z] in crop pixels) and the face-presence probability.
    pub fn landmarks(&self, rgb: &[u8]) -> Result<(Vec<[f32; 3]>, f32)> {
        ensure!(
            rgb.len() == MESH_SIZE * MESH_SIZE * 3,
            "bad rgb len {} (want {})",
            rgb.len(),
            MESH_SIZE * MESH_SIZE * 3
        );
        // NHWC f32 in [0, 1] (verified: [0,1] gives presence ~1.0 on a live
        // face crop, [-1,1] does not).
        let mut nhwc = vec![0f32; MESH_SIZE * MESH_SIZE * 3];
        for p in 0..MESH_SIZE * MESH_SIZE {
            for c in 0..3 {
                nhwc[p * 3 + c] = rgb[p * 3 + c] as f32 / 255.0;
            }
        }
        let input = Tensor::from(
            tract_ndarray::Array4::from_shape_vec((1, MESH_SIZE, MESH_SIZE, 3), nhwc)
                .context("build mesh input")?,
        );
        let outs = self.model.run(tvec!(input.into_tvalue()))?;
        ensure!(outs.len() == 3, "expected 3 outputs, got {}", outs.len());
        let lv = outs[0].to_plain_array_view::<f32>()?; // [1,1,1,1434]
        let mut lm = Vec::with_capacity(NUM_LANDMARKS);
        for i in 0..NUM_LANDMARKS {
            lm.push([
                lv[[0, 0, 0, i * 3]],
                lv[[0, 0, 0, i * 3 + 1]],
                lv[[0, 0, 0, i * 3 + 2]],
            ]);
        }
        let logit = outs[1].to_plain_array_view::<f32>()?[[0, 0, 0, 0]];
        let presence = 1.0 / (1.0 + (-logit).exp());
        Ok((lm, presence))
    }
}

// ---------------------------------------------------------------- rendering

/// Shift one iris by `shift` (frame px) inside `frame_img`, copying the
/// result back through the person `mask` (8UC1, same size). Pixels inside
/// the iris disc sample from `pos - shift`; a feathered annulus softens the
/// edge; outside the box is untouched.
pub fn apply_shift(frame_img: &mut Mat, mask: &Mat, eye: &EyeGeom, shift: (f32, f32)) -> Result<()> {
    let (sx, sy) = shift;
    if sx.abs() < 0.05 && sy.abs() < 0.05 {
        return Ok(());
    }
    let r = eye.iris_radius.max(1.0);
    let blend = (0.5 * r).max(1.5);
    let reach = r + blend + sx.hypot(sy) + 2.0;
    let (cx, cy) = eye.iris;
    let x0 = ((cx - reach).floor() as i32).max(0);
    let y0 = ((cy - reach).floor() as i32).max(0);
    let x1 = ((cx + reach).ceil() as i32 + 1).min(frame_img.cols());
    let y1 = ((cy + reach).ceil() as i32 + 1).min(frame_img.rows());
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let bw = x1 - x0;
    let bh = y1 - y0;
    let mut mapx = Mat::new_rows_cols_with_default(bh, bw, CV_32FC1, Scalar::all(0.0))?;
    let mut mapy = Mat::new_rows_cols_with_default(bh, bw, CV_32FC1, Scalar::all(0.0))?;
    {
        let mx = mapx.data_bytes_mut()?;
        let my = mapy.data_bytes_mut()?;
        for y in 0..bh {
            for x in 0..bw {
                let px = x0 as f32 + x as f32;
                let py = y0 as f32 + y as f32;
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let wgt = if d <= r {
                    1.0
                } else if d >= r + blend {
                    0.0
                } else {
                    1.0 - (d - r) / blend
                };
                let off = ((y * bw + x) * 4) as usize;
                mx[off..off + 4].copy_from_slice(&(px - sx * wgt).to_le_bytes());
                my[off..off + 4].copy_from_slice(&(py - sy * wgt).to_le_bytes());
            }
        }
    }
    let mut shifted = Mat::default();
    imgproc::remap(
        frame_img,
        &mut shifted,
        &mapx,
        &mapy,
        imgproc::INTER_LINEAR,
        core::BORDER_REPLICATE,
        Scalar::all(0.0),
    )?;
    let rect = Rect::new(x0, y0, bw, bh);
    let src_roi = Mat::roi(frame_img, rect)?;
    let mask_roi = Mat::roi(mask, rect)?;
    // out = src * (1-m) + shifted * m, with m = mask/255 (person region
    // only) — same 8U blend as composite::alpha_composite, on the ROI.
    let mut mask3 = Mat::default();
    imgproc::cvt_color(
        &mask_roi,
        &mut mask3,
        imgproc::COLOR_GRAY2BGR,
        0,
        core::AlgorithmHint::ALGO_HINT_DEFAULT,
    )?;
    let mut inv3 = Mat::default();
    core::bitwise_not(&mask3, &mut inv3, &Mat::default())?;
    let mut a = Mat::default();
    core::multiply(&src_roi, &inv3, &mut a, 1.0 / 255.0, -1)?;
    let mut b = Mat::default();
    core::multiply(&shifted, &mask3, &mut b, 1.0 / 255.0, -1)?;
    let mut blended = Mat::default();
    core::add(&a, &b, &mut blended, &Mat::default(), -1)?;
    let mut dst_roi = Mat::roi_mut(frame_img, rect)?;
    core::copy_to(&blended, &mut dst_roi, &Mat::default())?;
    Ok(())
}

// ---------------------------------------------------------------- corrector

/// Holds both models plus the smoothed per-eye state. Single face (largest
/// box) only — MVP is one user.
pub struct GazeCorrector {
    det: Detector,
    mesh: FaceMesh,
    frame_no: u64,
    /// Last detected face box, frame px (x1, y1, x2, y2).
    face_box: Option<[f32; 4]>,
    /// Last per-eye geometry, frame px (updated on mesh frames).
    eyes: [Option<EyeGeom>; 2],
    smoothers: [ShiftSmoother; 2],
    /// Last applied per-eye shift, frame px (for --self-test reporting).
    pub last_shifts: [Option<(f32, f32)>; 2],
}

impl GazeCorrector {
    pub fn load(det_path: &Path, mesh_path: &Path) -> Result<Self> {
        Ok(Self {
            det: Detector::load(det_path)
                .with_context(|| format!("load detector {}", det_path.display()))?,
            mesh: FaceMesh::load(mesh_path)
                .with_context(|| format!("load face mesh {}", mesh_path.display()))?,
            frame_no: 0,
            face_box: None,
            eyes: [None, None],
            smoothers: [ShiftSmoother::new(SMOOTH_ALPHA), ShiftSmoother::new(SMOOTH_ALPHA)],
            last_shifts: [None, None],
        })
    }

    /// Run detection (every frame) + mesh (every MESH_EVERY frames) on `img`
    /// (BGR full-res) and apply the smoothed pupil shift to `out` (BGR
    /// full-res, modified in place) through the person `mask` (8UC1).
    pub fn process(&mut self, img: &Mat, out: &mut Mat, mask: &Mat, strength: f32) -> Result<()> {
        let (w, h) = (img.cols() as u32, img.rows() as u32);
        self.frame_no += 1;

        // Detect every frame on a 128x128 resize of the input.
        let small = frame::resize(img, DETECT_SIZE as u32, DETECT_SIZE as u32)?;
        let rgb = frame::bgr_to_rgb(&small)?;
        let min_face = MIN_FACE_PX_AT_720 * h as f32 / 720.0;
        self.face_box = match self.det.largest_face(&rgb)? {
            Some(b) => {
                // [y1, x1, y2, x2] normalized -> (x1, y1, x2, y2) frame px.
                let bx = [b[1] * w as f32, b[0] * h as f32, b[3] * w as f32, b[2] * h as f32];
                if (bx[2] - bx[0]).min(bx[3] - bx[1]) < min_face {
                    None
                } else {
                    Some(bx)
                }
            }
            None => None,
        };
        if self.face_box.is_none() {
            self.eyes = [None, None];
            self.smoothers.iter_mut().for_each(ShiftSmoother::reset);
            self.last_shifts = [None, None];
            return Ok(());
        }
        let face = self.face_box.unwrap();

        // Mesh on the expanded square face crop every MESH_EVERY frames.
        if self.frame_no % MESH_EVERY == 1 {
            let cx = (face[0] + face[2]) / 2.0;
            let cy = (face[1] + face[3]) / 2.0;
            let side = (face[2] - face[0]).max(face[3] - face[1]) * CROP_EXPAND;
            let x0 = ((cx - side / 2.0).max(0.0)) as i32;
            let y0 = ((cy - side / 2.0).max(0.0)) as i32;
            let rw = (side as i32).min(w as i32 - x0);
            let rh = (side as i32).min(h as i32 - y0);
            if rw < 8 || rh < 8 {
                return Ok(());
            }
            let roi = Mat::roi(img, Rect::new(x0, y0, rw, rh))?;
            let mut crop = Mat::default();
            imgproc::resize(
                &roi,
                &mut crop,
                opencv::core::Size::new(MESH_SIZE as i32, MESH_SIZE as i32),
                0.0,
                0.0,
                imgproc::INTER_LINEAR,
            )?;
            let rgb = frame::bgr_to_rgb(&crop)?;
            let (lm, presence) = self.mesh.landmarks(&rgb)?;
            if presence < MIN_PRESENCE {
                return Ok(()); // keep previous state
            }
            // Crop px -> frame px.
            let sx = rw as f32 / MESH_SIZE as f32;
            let sy = rh as f32 / MESH_SIZE as f32;
            let lmf: Vec<[f32; 3]> = lm
                .iter()
                .map(|p| [x0 as f32 + p[0] * sx, y0 as f32 + p[1] * sy, p[2]])
                .collect();
            if face_roll_deg(&lmf) > MAX_ROLL_DEG {
                return Ok(());
            }
            let img_center = (w as f32 / 2.0, h as f32 / 2.0);
            for (i, side_e) in [EyeSide::Left, EyeSide::Right].into_iter().enumerate() {
                let Some(geom) = eye_geom(&lmf, side_e) else {
                    continue;
                };
                if eye_aspect(&geom) < MIN_EYE_ASPECT {
                    continue; // blink
                }
                let raw = target_shift(&geom, img_center, strength, h);
                let sm = &mut self.smoothers[i];
                if sm.observe(raw) {
                    self.last_shifts[i] = Some(sm.update(raw));
                }
                self.eyes[i] = Some(geom);
            }
        }

        // Apply the smoothed shifts through the person mask.
        for i in 0..2 {
            if let (Some(geom), Some(shift)) = (self.eyes[i], self.last_shifts[i]) {
                apply_shift(out, mask, &geom, shift)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use opencv::core::{Scalar, CV_8UC1, CV_8UC3};

    /// Synthetic 478-landmark set with both eyes level at y=100, left eye
    /// centered at x=50 (corners 40/60, top 95, bottom 105, iris at
    /// `left_iris`), right eye centered at x=150 (corners 138/162, iris at
    /// `right_iris`). All coords in "frame px".
    fn synth_landmarks(left_iris: (f32, f32), right_iris: (f32, f32)) -> Vec<[f32; 3]> {
        let mut lm = vec![[0f32; 3]; NUM_LANDMARKS];
        let set = |lm: &mut Vec<[f32; 3]>, i: usize, x: f32, y: f32| lm[i] = [x, y, 0.0];
        // left eye (33 outer, 133 inner, 159 top, 145 bottom)
        set(&mut lm, LM_LEFT_OUTER, 40.0, 100.0);
        set(&mut lm, LM_LEFT_INNER, 60.0, 100.0);
        set(&mut lm, LM_LEFT_TOP, 50.0, 95.0);
        set(&mut lm, LM_LEFT_BOTTOM, 50.0, 105.0);
        set(&mut lm, LM_LEFT_IRIS, left_iris.0, left_iris.1);
        for r in 469..=472 {
            set(&mut lm, r, left_iris.0 + 3.0, left_iris.1);
        }
        // right eye (263 outer, 362 inner, 386 top, 374 bottom)
        set(&mut lm, LM_RIGHT_OUTER, 162.0, 100.0);
        set(&mut lm, LM_RIGHT_INNER, 138.0, 100.0);
        set(&mut lm, LM_RIGHT_TOP, 150.0, 95.0);
        set(&mut lm, LM_RIGHT_BOTTOM, 150.0, 105.0);
        set(&mut lm, LM_RIGHT_IRIS, right_iris.0, right_iris.1);
        for r in 474..=477 {
            set(&mut lm, r, right_iris.0 + 3.0, right_iris.1);
        }
        lm
    }

    #[test]
    fn eye_geom_reads_corners_and_iris() {
        let lm = synth_landmarks((50.0, 100.0), (150.0, 100.0));
        let e = eye_geom(&lm, EyeSide::Left).unwrap();
        assert_eq!(e.center, (50.0, 100.0));
        assert!((e.width - 20.0).abs() < 1e-4);
        assert!((e.height - 10.0).abs() < 1e-4);
        assert_eq!(e.iris, (50.0, 100.0));
        assert!((e.iris_radius - 3.0).abs() < 1e-4);
        let r = eye_geom(&lm, EyeSide::Right).unwrap();
        assert_eq!(r.center, (150.0, 100.0));
        assert!((r.width - 24.0).abs() < 1e-4);
    }

    #[test]
    fn eye_geom_rejects_degenerate_eye() {
        let mut lm = synth_landmarks((50.0, 100.0), (150.0, 100.0));
        lm[LM_LEFT_OUTER] = lm[LM_LEFT_INNER]; // zero width
        assert!(eye_geom(&lm, EyeSide::Left).is_none());
    }

    #[test]
    fn target_shift_is_zero_when_centered_on_image_center() {
        // eye centered at the image center with the iris in the middle
        let e = EyeGeom {
            center: (100.0, 100.0),
            width: 20.0,
            height: 10.0,
            iris: (100.0, 100.0),
            iris_radius: 3.0,
        };
        let (dx, dy) = target_shift(&e, (100.0, 100.0), 0.5, 200);
        assert!(dx.abs() < 1e-4 && dy.abs() < 1e-4, "got ({dx}, {dy})");
    }

    #[test]
    fn target_shift_moves_iris_toward_eye_center() {
        // iris 20 px right of the eye center -> shift.x negative (moves left),
        // dominating the small image-center bias (eye near the image center)
        let lm = synth_landmarks((70.0, 100.0), (150.0, 100.0));
        let e = eye_geom(&lm, EyeSide::Left).unwrap();
        let (dx, dy) = target_shift(&e, (60.0, 100.0), 1.0, 720);
        assert!(dx < 0.0, "dx {dx} should be negative");
        assert!(dy.abs() < 1e-4, "dy {dy} should be ~0 (level eyes)");
    }

    #[test]
    fn target_shift_respects_hard_cap() {
        // iris far off-center: raw offset would be huge, cap must bind
        let lm = synth_landmarks((90.0, 100.0), (150.0, 100.0));
        let e = eye_geom(&lm, EyeSide::Left).unwrap(); // width 20
        let cap = max_shift_px(e.width, 720);
        assert!((cap - 3.5).abs() < 1e-4, "cap at 720p = 3.5 * h/720, got {cap}");
        let (dx, _) = target_shift(&e, (500.0, 500.0), 1.0, 720);
        assert!(dx.abs() <= cap + 1e-4, "dx {dx} exceeds cap {cap}");
    }

    #[test]
    fn target_shift_scales_with_strength_and_zero_at_zero() {
        let lm = synth_landmarks((54.0, 100.0), (150.0, 100.0));
        let e = eye_geom(&lm, EyeSide::Left).unwrap();
        let (dx0, _) = target_shift(&e, (500.0, 500.0), 0.0, 720);
        assert_eq!((dx0, 0.0), (0.0, 0.0));
        let (dx1, _) = target_shift(&e, (500.0, 500.0), 1.0, 720);
        let (dxh, _) = target_shift(&e, (500.0, 500.0), 0.5, 720);
        assert!((dxh - dx1 * 0.5).abs() < 1e-3, "half strength: {dxh} vs {dx1}/2");
    }

    #[test]
    fn max_shift_px_uses_eye_width_below_the_720p_cap() {
        // wide eye at 1080p: 0.25 * 100 = 25 vs 3.5 * 1.5 = 5.25 -> cap 5.25
        assert!((max_shift_px(100.0, 1080) - 5.25).abs() < 1e-4);
        // narrow eye: 0.25 * 8 = 2 < 3.5 -> cap 2
        assert!((max_shift_px(8.0, 720) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn face_roll_detects_tilt() {
        let mut lm = synth_landmarks((50.0, 100.0), (150.0, 100.0));
        assert!(face_roll_deg(&lm) < 0.1);
        // tilt the right eye down by ~30 deg: dx=100, dy=tan(30)*100
        lm[LM_RIGHT_OUTER][1] = 100.0 + 100.0 * 30f32.to_radians().tan();
        lm[LM_RIGHT_INNER][1] = 100.0 + 100.0 * 30f32.to_radians().tan();
        let roll = face_roll_deg(&lm);
        assert!((roll - 30.0).abs() < 0.5, "roll {roll}");
    }

    #[test]
    fn eye_aspect_gate_catches_blink() {
        let mut lm = synth_landmarks((50.0, 100.0), (150.0, 100.0));
        let e = eye_geom(&lm, EyeSide::Left).unwrap();
        assert!(eye_aspect(&e) >= MIN_EYE_ASPECT); // 10/20 = 0.5
        lm[LM_LEFT_TOP] = [50.0, 99.0, 0.0];
        lm[LM_LEFT_BOTTOM] = [50.0, 101.0, 0.0]; // height 2 -> aspect 0.1
        let e = eye_geom(&lm, EyeSide::Left).unwrap();
        assert!(eye_aspect(&e) < MIN_EYE_ASPECT);
    }

    #[test]
    fn smoother_ema_and_deadband() {
        let mut s = ShiftSmoother::new(0.5);
        assert_eq!(s.update((10.0, 0.0)), (10.0, 0.0));
        // within deadband -> state unchanged
        assert_eq!(s.update((10.3, 0.0)), (10.0, 0.0));
        // outside deadband -> EMA
        assert_eq!(s.update((11.0, 1.0)), (10.5, 0.5));
    }

    #[test]
    fn smoother_jitter_gate() {
        let mut s = ShiftSmoother::new(0.5);
        assert!(s.observe((0.0, 0.0)));
        assert!(s.observe((0.1, 0.0)));
        assert!(s.jitter_var() < JITTER_VAR_MAX);
        // wild jumps trip the gate
        for v in [10.0, -10.0, 10.0, -10.0, 10.0] {
            assert!(!s.observe((v, 0.0)), "jitter should trip at {v}");
        }
        assert!(s.jitter_var() > JITTER_VAR_MAX);
        s.reset();
        // after reset the EMA starts fresh from the next raw measurement
        assert_eq!(s.update((1.0, 1.0)), (1.0, 1.0));
        assert!(s.observe((1.0, 1.0)));
    }

    /// 64x64 black image with a white 6-px-radius disc at (32, 32).
    fn disc_image() -> (Mat, Mat) {
        let mut img = Mat::new_rows_cols_with_default(64, 64, CV_8UC3, Scalar::all(0.0)).unwrap();
        imgproc::circle(
            &mut img,
            opencv::core::Point::new(32, 32),
            6,
            Scalar::all(255.0),
            -1,
            imgproc::LINE_8,
            0,
        )
        .unwrap();
        let mask = Mat::new_rows_cols_with_default(64, 64, CV_8UC1, Scalar::all(255.0)).unwrap();
        (img, mask)
    }

    fn centroid(img: &Mat) -> (f32, f32) {
        let d = img.data_bytes().unwrap();
        let mut sx = 0f64;
        let mut sy = 0f64;
        let mut n = 0f64;
        for y in 0..img.rows() {
            for x in 0..img.cols() {
                let i = ((y * img.cols() + x) * 3) as usize;
                if d[i] > 128 {
                    sx += x as f64;
                    sy += y as f64;
                    n += 1.0;
                }
            }
        }
        ((sx / n) as f32, (sy / n) as f32)
    }

    #[test]
    fn apply_shift_moves_iris_disc() {
        let (mut img, mask) = disc_image();
        let eye = EyeGeom {
            center: (32.0, 32.0),
            width: 20.0,
            height: 10.0,
            iris: (32.0, 32.0),
            iris_radius: 6.0,
        };
        let before = centroid(&img);
        apply_shift(&mut img, &mask, &eye, (3.0, 0.0)).unwrap();
        let after = centroid(&img);
        // the feathered annulus moves less than the disc core, so the
        // thresholded centroid lands a bit short of the full 3 px
        let moved = after.0 - before.0;
        assert!(
            (1.5..=3.2).contains(&moved),
            "disc should move ~3 px right: before {before:?} after {after:?}"
        );
        assert!(after.1 - before.1 < 0.3, "no vertical drift: {before:?} {after:?}");
    }

    #[test]
    fn apply_shift_respects_zero_mask() {
        let (mut img, _) = disc_image();
        let mask = Mat::new_rows_cols_with_default(64, 64, CV_8UC1, Scalar::all(0.0)).unwrap();
        let eye = EyeGeom {
            center: (32.0, 32.0),
            width: 20.0,
            height: 10.0,
            iris: (32.0, 32.0),
            iris_radius: 6.0,
        };
        let before = img.data_bytes().unwrap().to_vec();
        apply_shift(&mut img, &mask, &eye, (3.0, 0.0)).unwrap();
        assert_eq!(img.data_bytes().unwrap(), &before[..], "mask=0 must not touch the frame");
    }

    #[test]
    fn apply_shift_noop_for_tiny_shift() {
        let (mut img, mask) = disc_image();
        let eye = EyeGeom {
            center: (32.0, 32.0),
            width: 20.0,
            height: 10.0,
            iris: (32.0, 32.0),
            iris_radius: 6.0,
        };
        let before = img.data_bytes().unwrap().to_vec();
        apply_shift(&mut img, &mask, &eye, (0.01, 0.0)).unwrap();
        assert_eq!(img.data_bytes().unwrap(), &before[..]);
    }

    fn bundled(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name)
    }

    /// Real-model shape/range test: the bundled detector and mesh load, run
    /// on uniform input, and return sane output shapes.
    #[test]
    fn bundled_models_run_on_uniform_input() {
        let det = Detector::load(&bundled("blaze_face_short_range.onnx")).expect("detector");
        let rgb = vec![128u8; DETECT_SIZE * DETECT_SIZE * 3];
        let face = det.largest_face(&rgb).expect("detect");
        assert!(face.is_none(), "uniform gray should not detect a face");

        let mesh = FaceMesh::load(&bundled("face_landmarks_478.onnx")).expect("mesh");
        let rgb = vec![128u8; MESH_SIZE * MESH_SIZE * 3];
        let (lm, presence) = mesh.landmarks(&rgb).expect("landmarks");
        assert_eq!(lm.len(), NUM_LANDMARKS);
        assert!((0.0..=1.0).contains(&presence), "presence {presence}");
    }

    #[test]
    fn detector_rejects_bad_buffer() {
        let det = Detector::load(&bundled("blaze_face_short_range.onnx")).expect("detector");
        assert!(det.largest_face(&[0u8; 10]).is_err());
    }

    #[test]
    fn mesh_rejects_bad_buffer() {
        let mesh = FaceMesh::load(&bundled("face_landmarks_478.onnx")).expect("mesh");
        assert!(mesh.landmarks(&[]).is_err());
    }
}
