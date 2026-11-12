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

/// Default background when --background is not given: solid #003200, so
/// --mode replace works out of the box. Mats are BGR, but R and B are both
/// 0 in #003200, so the BGR triple is (0, 0x32, 0).
pub fn default_background(w: u32, h: u32) -> Result<Mat> {
    Mat::new_rows_cols_with_default(
        h as i32,
        w as i32,
        CV_8UC3,
        Scalar::new(0.0, 0x32 as f64, 0.0, 0.0),
    )
    .context("allocate background")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-color BGR image.
    fn solid(w: i32, h: i32, bgr: [u8; 3]) -> Mat {
        Mat::new_rows_cols_with_default(
            h,
            w,
            CV_8UC3,
            Scalar::new(bgr[0] as f64, bgr[1] as f64, bgr[2] as f64, 0.0),
        )
        .unwrap()
    }

    /// 64x64 BGR image, left half black, right half white (edge at x=32).
    fn step_edge() -> Mat {
        let mut img =
            Mat::new_rows_cols_with_default(64, 64, CV_8UC3, Scalar::all(0.0)).unwrap();
        let data = img.data_bytes_mut().unwrap();
        for y in 0..64 {
            for x in 32..64 {
                let i = (y * 64 + x) * 3;
                data[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        img
    }

    #[test]
    fn smoother_with_alpha_one_copies_hard_mask() {
        let mut s = MaskSmoother::new(4, 1.0);
        assert_eq!(s.update(&[255, 0, 255, 0]), &[1.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn smoother_ema_blends_toward_new_mask() {
        let mut s = MaskSmoother::new(1, 0.5);
        assert_eq!(s.update(&[255]), &[0.5]);
        assert_eq!(s.update(&[255]), &[0.75]);
        assert_eq!(s.update(&[0]), &[0.375]);
    }

    #[test]
    fn smoother_clamps_alpha_to_unit_range() {
        let mut hi = MaskSmoother::new(1, 5.0);
        assert_eq!(hi.update(&[255]), &[1.0]);
        let mut lo = MaskSmoother::new(1, -1.0);
        assert_eq!(lo.update(&[255]), &[0.0]);
    }

    #[test]
    fn feather_mask_dimensions_and_extremes() {
        let ones = vec![1.0f32; 16];
        let m = feather_mask(&ones, 4, 4, 8, 8).unwrap();
        assert_eq!(m.cols(), 8);
        assert_eq!(m.rows(), 8);
        assert_eq!(m.channels(), 1);
        assert!(m.data_bytes().unwrap().iter().all(|&b| b == 255));

        let zeros = vec![0.0f32; 16];
        let m0 = feather_mask(&zeros, 4, 4, 8, 8).unwrap();
        assert!(m0.data_bytes().unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn alpha_composite_full_mask_keeps_frame() {
        let frame = solid(4, 4, [10, 20, 30]);
        let layer = solid(4, 4, [200, 210, 220]);
        let mask = frame::mask_mat(&[255u8; 16], 4, 4).unwrap();
        let out = alpha_composite(&frame, &layer, &mask).unwrap();
        assert_eq!(out.data_bytes().unwrap(), frame.data_bytes().unwrap());
    }

    #[test]
    fn alpha_composite_empty_mask_gives_layer() {
        let frame = solid(4, 4, [10, 20, 30]);
        let layer = solid(4, 4, [200, 210, 220]);
        let mask = frame::mask_mat(&[0u8; 16], 4, 4).unwrap();
        let out = alpha_composite(&frame, &layer, &mask).unwrap();
        assert_eq!(out.data_bytes().unwrap(), layer.data_bytes().unwrap());
    }

    #[test]
    fn alpha_composite_half_mask_blends() {
        let frame = solid(2, 2, [200, 200, 200]);
        let layer = solid(2, 2, [100, 100, 100]);
        let mask = frame::mask_mat(&[128u8; 4], 2, 2).unwrap();
        let out = alpha_composite(&frame, &layer, &mask).unwrap();
        // 200*128/255 + 100*127/255 = 100.39 + 49.80 -> rounds to 100 + 50 = 150
        for &b in out.data_bytes().unwrap() {
            assert!((b as i32 - 150).abs() <= 1, "blend byte {b}, want ~150");
        }
    }

    #[test]
    fn replace_composite_full_mask_returns_frame() {
        let frame = solid(4, 4, [1, 2, 3]);
        let bg = solid(4, 4, [9, 9, 9]);
        let mask = frame::mask_mat(&[255u8; 16], 4, 4).unwrap();
        let out = replace_and_composite(&frame, &bg, &mask).unwrap();
        assert_eq!(out.data_bytes().unwrap(), frame.data_bytes().unwrap());
    }

    #[test]
    fn replace_composite_rejects_size_mismatch() {
        let frame = solid(4, 4, [1, 2, 3]);
        let bg = solid(8, 8, [9, 9, 9]);
        let mask = frame::mask_mat(&[255u8; 16], 4, 4).unwrap();
        assert!(replace_and_composite(&frame, &bg, &mask).is_err());
    }

    #[test]
    fn blur_composite_keeps_fully_masked_frame_sharp() {
        let frame = step_edge();
        let mask = frame::mask_mat(&[255u8; 64 * 64], 64, 64).unwrap();
        let out = blur_and_composite(&frame, &mask, 8.0).unwrap();
        // person region untouched even though the background layer is blurred
        assert_eq!(out.data_bytes().unwrap(), frame.data_bytes().unwrap());
    }

    #[test]
    fn blur_composite_softens_unmasked_background() {
        let frame = step_edge();
        let mask = frame::mask_mat(&[0u8; 64 * 64], 64, 64).unwrap();
        let out = blur_and_composite(&frame, &mask, 8.0).unwrap();
        let d = out.data_bytes().unwrap();
        let at = |x: usize| d[(32 * 64 + x) * 3] as i32;
        assert!(at(0) <= 4, "far left should stay black, got {}", at(0));
        assert!(at(63) >= 251, "far right should stay white, got {}", at(63));
        let mid = at(32);
        assert!(
            (60..=195).contains(&mid),
            "unmasked edge should be blurred, got {mid}"
        );
    }

    #[test]
    fn default_background_is_solid_003200() {
        let bg = default_background(8, 4).unwrap();
        assert_eq!(bg.cols(), 8);
        assert_eq!(bg.rows(), 4);
        assert_eq!(bg.channels(), 3);
        // #003200 RGB == BGR (0, 50, 0) because R and B are both zero.
        assert!(
            bg.data_bytes()
                .unwrap()
                .chunks_exact(3)
                .all(|px| px == [0, 50, 0]),
            "every pixel should be #003200"
        );
    }
}
