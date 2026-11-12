//! Frame helpers (opencv): MJPG decode, resize, RGB conversion, mask
//! rendering and JPEG encode.

use anyhow::{bail, Context, Result};
use opencv::core::{AlgorithmHint, Mat, Size, Vector, CV_8UC3};
use opencv::prelude::*;
use opencv::{imgcodecs, imgproc};

/// Decode MJPG (JPEG) bytes to a BGR Mat.
pub fn decode_mjpg(jpeg: &[u8]) -> Result<Mat> {
    let buf = Vector::<u8>::from_slice(jpeg);
    let img = imgcodecs::imdecode(&buf, imgcodecs::IMREAD_COLOR).context("imdecode")?;
    if img.empty() {
        bail!("imdecode returned an empty image");
    }
    Ok(img)
}

/// Encode a BGR Mat as JPEG with the given quality (1-100).
pub fn encode_jpeg(bgr: &Mat, quality: i32) -> Result<Vec<u8>> {
    let mut buf = Vector::<u8>::new();
    let params = Vector::<i32>::from_slice(&[imgcodecs::IMWRITE_JPEG_QUALITY, quality]);
    imgcodecs::imencode(".jpg", bgr, &mut buf, &params).context("imencode")?;
    Ok(buf.to_vec())
}

/// Resize to (w, h).
pub fn resize(src: &Mat, w: u32, h: u32) -> Result<Mat> {
    let mut dst = Mat::default();
    imgproc::resize(
        src,
        &mut dst,
        Size::new(w as i32, h as i32),
        0.0,
        0.0,
        imgproc::INTER_LINEAR,
    )?;
    Ok(dst)
}

/// Build an 8UC1 Mat (rows x cols, row-major) from a raw mask buffer.
pub fn mask_mat(data: &[u8], cols: u32, rows: u32) -> Result<Mat> {
    let mut m = Mat::new_rows_cols_with_default(
        rows as i32,
        cols as i32,
        opencv::core::CV_8UC1,
        opencv::core::Scalar::all(0.0),
    )?;
    m.data_bytes_mut()?.copy_from_slice(data);
    Ok(m)
}

/// BGR Mat -> tightly packed RGB bytes (input for the segmenter).
pub fn bgr_to_rgb(mat: &Mat) -> Result<Vec<u8>> {
    let mut rgb = Mat::default();
    imgproc::cvt_color(mat, &mut rgb, imgproc::COLOR_BGR2RGB, 0, AlgorithmHint::ALGO_HINT_DEFAULT)?;
    Ok(rgb.data_bytes()?.to_vec())
}

/// Upscale an 8UC1 mask to frame size (linear, keeps soft edges).
pub fn upscale_mask(mask: &Mat, w: u32, h: u32) -> Result<Mat> {
    resize(mask, w, h)
}

/// Overlay preview: the person is kept as-is, the background is dimmed to
/// 40% brightness and tinted blue. `mask`: 8UC1, same size as `frame`,
/// 255 = person.
pub fn render_overlay(frame: &Mat, mask: &Mat) -> Result<Mat> {
    let mut out = frame.try_clone()?;
    let mut bg = Mat::default();
    frame.convert_to(&mut bg, CV_8UC3, 0.4, 40.0)?; // BGR: +40 on the blue channel
    let mut mask_inv = Mat::default();
    imgproc::threshold(mask, &mut mask_inv, 127.0, 255.0, imgproc::THRESH_BINARY_INV)?;
    opencv::core::copy_to(&bg, &mut out, &mask_inv)?;
    Ok(out)
}

/// Pure mask preview: 8UC1 mask as a BGR image (white person on black).
pub fn render_mask(mask: &Mat) -> Result<Mat> {
    let mut out = Mat::default();
    imgproc::cvt_color(mask, &mut out, imgproc::COLOR_GRAY2BGR, 0, AlgorithmHint::ALGO_HINT_DEFAULT)?;
    Ok(out)
}

/// Write a Mat to an image file (png/jpg by extension).
pub fn write_image(path: &std::path::Path, img: &Mat) -> Result<()> {
    let p = path.to_str().context("non-UTF-8 output path")?;
    imgcodecs::imwrite(p, img, &Vector::<i32>::new())
        .with_context(|| format!("imwrite {}", path.display()))?;
    Ok(())
}
