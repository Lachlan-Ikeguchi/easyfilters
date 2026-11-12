//! Compositing (milestone 3): temporal mask smoothing (EMA), feathered mask
//! edges, background blur and background replace.
//!
//! Alpha blending is done in 8-bit with a scaled multiply:
//! out = saturate(frame * mask/255) + saturate(layer * (255-mask)/255),
//! where `mask` is the soft (unthresholded) person mask at full frame
//! resolution, replicated to 3 channels. Staying in 8U keeps compositing in
//! the low-ms range at 720p (f32 blending measured ~27 ms/frame).

use anyhow::{bail, Context, Result};
use opencv::core::{Mat, Scalar, CV_8UC1, CV_8UC3};
use opencv::prelude::*;
use opencv::{core, imgproc};

use crate::frame;

/// Downsample factor for the blurred background layer. Blurring at reduced
/// resolution is visually indistinguishable for a background (the person is
/// composited sharp on top) and keeps the blur itself in the ~1 ms range.
const BLUR_DOWNSAMPLE: u32 = 4;

/// EMA smoother over the model-resolution person mask. `state` holds the
/// soft mask (0..1) and is blended with each new hard inference mask:
/// state = alpha * new + (1 - alpha) * state.
pub struct MaskSmoother {
    state: Vec<f32>,
    alpha: f32,
}

impl MaskSmoother {
    pub fn new(len: usize, alpha: f32) -> Self {
        Self {
            state: vec![0.0; len],
            alpha: alpha.clamp(0.0, 1.0),
        }
    }

    /// Blend a hard mask (255 = person) into the state; returns the soft mask.
    pub fn update(&mut self, hard: &[u8]) -> &[f32] {
        assert_eq!(hard.len(), self.state.len());
        for (s, &m) in self.state.iter_mut().zip(hard) {
            let v = m as f32 / 255.0;
            *s = self.alpha * v + (1.0 - self.alpha) * *s;
        }
        &self.state
    }
}

/// Soft f32 mask (model resolution) -> feathered full-res 8UC1 mask (w x h).
/// Feather = small Gaussian at model res, then linear upscale (the upscale
/// softens edges further).
pub fn feather_mask(soft: &[f32], mw: usize, mh: usize, w: u32, h: u32) -> Result<Mat> {
    let mut m = Mat::new_rows_cols_with_default(mh as i32, mw as i32, CV_8UC1, Scalar::all(0.0))?;
    let data = m.data_bytes_mut()?;
    for (d, &s) in data.iter_mut().zip(soft) {
        *d = (s.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    let blurred = frame::gaussian_blur(&m, 1.5)?;
    frame::resize(&blurred, w, h)
}

/// Alpha composite in 8U: out = frame*mask/255 + layer*(255-mask)/255.
/// `frame`/`layer`: 8UC3 same size; `mask`: 8UC1 same size, 0..255.
pub fn alpha_composite(frame: &Mat, layer: &Mat, mask: &Mat) -> Result<Mat> {
    let mut mask3 = Mat::default();
    imgproc::cvt_color(
        mask,
        &mut mask3,
        imgproc::COLOR_GRAY2BGR,
        0,
        opencv::core::AlgorithmHint::ALGO_HINT_DEFAULT,
    )?;
    let mut inv3 = Mat::default();
    let empty = Mat::default();
    core::bitwise_not(&mask3, &mut inv3, &empty)?;
    let mut a = Mat::default();
    core::multiply(frame, &mask3, &mut a, 1.0 / 255.0, -1)?;
    let mut b = Mat::default();
    core::multiply(layer, &inv3, &mut b, 1.0 / 255.0, -1)?;
    let mut out = Mat::default();
    core::add(&a, &b, &mut out, &empty, -1)?;
    Ok(out)
}

/// Blurred background layer: downscale, Gaussian blur, upscale back.
/// `sigma` is the full-resolution sigma (scaled down with the layer).
fn blurred_layer(frame: &Mat, sigma: f64) -> Result<Mat> {
    let (w, h) = (frame.cols() as u32, frame.rows() as u32);
    let sw = (w / BLUR_DOWNSAMPLE).max(64);
    let sh = (h / BLUR_DOWNSAMPLE).max(36);
    let small = frame::resize(frame, sw, sh)?;
    let blurred = frame::gaussian_blur(&small, sigma / BLUR_DOWNSAMPLE as f64)?;
    frame::resize(&blurred, w, h)
}

/// --mode blur: person sharp, background Gaussian-blurred.
pub fn blur_and_composite(frame: &Mat, mask_full: &Mat, sigma: f64) -> Result<Mat> {
    let layer = blurred_layer(frame, sigma)?;
    alpha_composite(frame, &layer, mask_full)
}

/// --mode replace: person sharp, background replaced by `bg`.
pub fn replace_and_composite(frame: &Mat, bg: &Mat, mask_full: &Mat) -> Result<Mat> {
    if bg.cols() != frame.cols() || bg.rows() != frame.rows() {
        bail!(
            "background is {}x{}, frame is {}x{}",
            bg.cols(),
            bg.rows(),
            frame.cols(),
            frame.rows()
        );
    }
    alpha_composite(frame, bg, mask_full)
}

/// Default background when --background is not given: a vertical dark
/// gradient (BGR), so --mode replace works out of the box.
pub fn default_background(w: u32, h: u32) -> Result<Mat> {
    let mut m = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0))
        .context("allocate background")?;
    let top: [f64; 3] = [70.0, 52.0, 40.0]; // BGR
    let bottom: [f64; 3] = [28.0, 22.0, 18.0];
    let data = m.data_bytes_mut()?;
    let row_bytes = w as usize * 3;
    for y in 0..h as usize {
        let t = if h > 1 { y as f64 / (h as usize - 1) as f64 } else { 0.0 };
        let px = [
            (top[0] + (bottom[0] - top[0]) * t) as u8,
            (top[1] + (bottom[1] - top[1]) * t) as u8,
            (top[2] + (bottom[2] - top[2]) * t) as u8,
        ];
        for c in data[y * row_bytes..(y + 1) * row_bytes].chunks_exact_mut(3) {
            c[0] = px[0];
            c[1] = px[1];
            c[2] = px[2];
        }
    }
    Ok(m)
}
