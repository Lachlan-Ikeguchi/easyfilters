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

#[cfg(test)]
mod tests {
    use super::*;
    use opencv::core::Scalar;

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

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_mjpg(b"this is not a jpeg").is_err());
    }

    #[test]
    fn jpeg_roundtrip_preserves_size_and_color() {
        let img = solid(64, 48, [30, 120, 200]);
        let jpeg = encode_jpeg(&img, 95).unwrap();
        let back = decode_mjpg(&jpeg).unwrap();
        assert_eq!(back.cols(), 64);
        assert_eq!(back.rows(), 48);
        // JPEG is lossy; allow a small per-channel tolerance.
        let px = &back.data_bytes().unwrap()[..3];
        for (got, want) in px.iter().zip([30u8, 120, 200]) {
            assert!(
                (*got as i32 - want as i32).abs() <= 4,
                "pixel {px:?} too far from {want}"
            );
        }
    }

    #[test]
    fn resize_produces_requested_dimensions() {
        let img = solid(100, 50, [1, 2, 3]);
        let out = resize(&img, 32, 16).unwrap();
        assert_eq!(out.cols(), 32);
        assert_eq!(out.rows(), 16);
    }

    #[test]
    fn bgr_to_rgb_swaps_channel_order() {
        let img = solid(2, 2, [10, 20, 30]);
        let rgb = bgr_to_rgb(&img).unwrap();
        assert_eq!(rgb.len(), 2 * 2 * 3);
        assert_eq!(&rgb[..3], &[30, 20, 10]);
    }

    #[test]
    fn mask_mat_copies_buffer() {
        let data = [1u8, 2, 3, 4, 5, 6];
        let m = mask_mat(&data, 3, 2).unwrap();
        assert_eq!(m.cols(), 3);
        assert_eq!(m.rows(), 2);
        assert_eq!(m.data_bytes().unwrap(), &data);
    }

    #[test]
    fn upscale_mask_changes_dimensions() {
        let m = mask_mat(&[255u8; 8], 4, 2).unwrap();
        let up = upscale_mask(&m, 8, 6).unwrap();
        assert_eq!(up.cols(), 8);
        assert_eq!(up.rows(), 6);
        assert_eq!(up.channels(), 1);
    }

    #[test]
    fn render_overlay_with_full_mask_keeps_frame() {
        let frame = solid(4, 4, [100, 150, 200]);
        let mask = mask_mat(&[255u8; 16], 4, 4).unwrap();
        let out = render_overlay(&frame, &mask).unwrap();
        assert_eq!(out.data_bytes().unwrap(), frame.data_bytes().unwrap());
    }

    #[test]
    fn render_overlay_with_empty_mask_dims_background() {
        let frame = solid(2, 2, [50, 100, 200]);
        let mask = mask_mat(&[0u8; 4], 2, 2).unwrap();
        let out = render_overlay(&frame, &mask).unwrap();
        // convert_to(0.4, 40.0) applies 0.4*px + 40 to every channel.
        assert_eq!(&out.data_bytes().unwrap()[..3], &[60, 80, 120]);
    }

    #[test]
    fn render_mask_expands_gray_to_bgr() {
        let mask = mask_mat(&[255u8, 0], 2, 1).unwrap();
        let out = render_mask(&mask).unwrap();
        assert_eq!(out.channels(), 3);
        let d = out.data_bytes().unwrap();
        assert_eq!(&d[..3], &[255, 255, 255]);
        assert_eq!(&d[3..6], &[0, 0, 0]);
    }

    #[test]
    fn write_image_roundtrips_png_losslessly() {
        let img = solid(8, 6, [11, 22, 33]);
        let path = std::env::temp_dir().join(format!(
            "easyfilters-test-{}-write.png",
            std::process::id()
        ));
        write_image(&path, &img).unwrap();
        let back =
            imgcodecs::imread(path.to_str().unwrap(), imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(back.cols(), 8);
        assert_eq!(back.rows(), 6);
        assert_eq!(back.data_bytes().unwrap(), img.data_bytes().unwrap());
        std::fs::remove_file(&path).unwrap();
    }
}
