//! Probe the bundled face models on an image to pin down their exact I/O:
//! blaze_face_short_range.onnx (detector) and face_landmarks_478.onnx (mesh).
//!
//! Usage: cargo run --release --example probe_face -- [image.jpg]

use std::collections::HashMap;

use opencv::prelude::*;
use std::sync::Arc;
use tract_onnx::prelude::*;

fn load_runnable(path: &str) -> TractResult<Arc<TypedRunnableModel>> {
    let model = tract_onnx::onnx().model_for_path(path)?;
    let typed = model.into_typed()?;
    // Bind symbolic dims (batch) to 1, same pattern as Segmenter::load.
    let subs: HashMap<Symbol, TDim> = typed
        .symbols
        .all_symbols()
        .into_iter()
        .map(|s| (s, TDim::from(1usize)))
        .collect();
    let typed = typed.set_symbols(&subs)?;
    for i in 0..typed.inputs.len() {
        println!("{path} input {i}: {:?}", typed.input_fact(i));
    }
    for i in 0..typed.outputs.len() {
        println!("{path} output {i}: {:?}", typed.output_fact(i));
    }
    let typed = typed.into_optimized()?;
    Ok(typed.into_runnable()?)
}

fn main() -> TractResult<()> {
    let img_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/face.jpg".to_string());
    let img = opencv::imgcodecs::imread(&img_path, opencv::imgcodecs::IMREAD_COLOR)?;
    let (w, h) = (img.cols() as usize, img.rows() as usize);
    println!("image {img_path}: {w}x{h}");

    // ---- detector on a 128x128 RGB resize, trying [-1,1] normalization ----
    let det = load_runnable("assets/blaze_face_short_range.onnx")?;
    let mut small = opencv::core::Mat::default();
    opencv::imgproc::resize(
        &img,
        &mut small,
        opencv::core::Size::new(128, 128),
        0.0,
        0.0,
        opencv::imgproc::INTER_LINEAR,
    )?;
    let mut rgb = opencv::core::Mat::default();
    opencv::imgproc::cvt_color(&small, &mut rgb, opencv::imgproc::COLOR_BGR2RGB, 0, opencv::core::AlgorithmHint::ALGO_HINT_DEFAULT)?;
    let rgb_bytes = rgb.data_bytes()?;
    for (norm, conf) in [("minus1to1", 0.5f32), ("zeroto1", 0.5), ("zeroto255", 0.5), ("minus1to1", 0.05), ("zeroto1", 0.05)] {
        let mut chw = vec![0f32; 3 * 128 * 128];
        for p in 0..128 * 128 {
            for c in 0..3 {
                let raw = rgb_bytes[p * 3 + c] as f32;
                chw[c * 128 * 128 + p] = match norm {
                    "minus1to1" => raw / 255.0 * 2.0 - 1.0,
                    "zeroto1" => raw / 255.0,
                    _ => raw,
                };
            }
        }
        let img_t = Tensor::from(tract_ndarray::Array4::from_shape_vec((1, 3, 128, 128), chw)?);
        let conf_t = Tensor::from(tract_ndarray::Array1::from_elem([1], conf));
        let max_det = Tensor::from(tract_ndarray::Array1::from_elem([1], 10i64));
        let iou = Tensor::from(tract_ndarray::Array1::from_elem([1], 0.3f32));
        let outs = det.run(tvec!(img_t.into_tvalue(), conf_t.into_tvalue(), max_det.into_tvalue(), iou.into_tvalue()))?;
        let view = outs[0].to_plain_array_view::<f32>()?;
        println!("det norm={norm} conf={conf} out shape {:?}", view.shape());
        for row in view.axis_iter(tract_ndarray::Axis(1)) {
            let v: Vec<f32> = row.iter().copied().collect();
            println!("  row: {:?}", &v[..8]);
        }
    }

    // capture the best detector row (conf 0.5, minus1to1) for the mesh crop
    let mut best_row: Option<Vec<f32>> = None;
    {
        let mut chw = vec![0f32; 3 * 128 * 128];
        for p in 0..128 * 128 {
            for c in 0..3 {
                chw[c * 128 * 128 + p] = rgb_bytes[p * 3 + c] as f32 / 255.0 * 2.0 - 1.0;
            }
        }
        let img_t = Tensor::from(tract_ndarray::Array4::from_shape_vec((1, 3, 128, 128), chw)?);
        let conf_t = Tensor::from(tract_ndarray::Array1::from_elem([1], 0.5f32));
        let max_det = Tensor::from(tract_ndarray::Array1::from_elem([1], 10i64));
        let iou = Tensor::from(tract_ndarray::Array1::from_elem([1], 0.3f32));
        let outs = det.run(tvec!(img_t.into_tvalue(), conf_t.into_tvalue(), max_det.into_tvalue(), iou.into_tvalue()))?;
        let view = outs[0].to_plain_array_view::<f32>()?;
        let mut best_area = 0f32;
        for row in view.axis_iter(tract_ndarray::Axis(1)) {
            let v: Vec<f32> = row.iter().copied().collect();
            let area = v[2] * v[3];
            if area > best_area && v[2] > 0.0 && v[3] > 0.0 {
                best_area = area;
                best_row = Some(v);
            }
        }
    }
    println!("best det row: {:?}", best_row.as_ref().map(|v| &v[..]));

    // ---- person bbox from the bundled segmenter, to locate the face ----
    let seg = load_runnable("assets/selfie_segmentation_landscape.onnx")?;
    let mut ssmall = opencv::core::Mat::default();
    opencv::imgproc::resize(&img, &mut ssmall, opencv::core::Size::new(256, 144), 0.0, 0.0, opencv::imgproc::INTER_LINEAR)?;
    let mut srgb = opencv::core::Mat::default();
    opencv::imgproc::cvt_color(&ssmall, &mut srgb, opencv::imgproc::COLOR_BGR2RGB, 0, opencv::core::AlgorithmHint::ALGO_HINT_DEFAULT)?;
    let sb = srgb.data_bytes()?;
    let mut chw = vec![0f32; 3 * 256 * 144];
    for p in 0..256 * 144 {
        for c in 0..3 {
            chw[c * 256 * 144 + p] = (sb[p * 3 + c] as f32 / 255.0 - 0.5) * 2.0;
        }
    }
    let sin = Tensor::from(tract_ndarray::Array4::from_shape_vec((1, 3, 144, 256), chw)?);
    let seg_out = seg.run(tvec!(sin.into_tvalue()))?;
    let mv = seg_out[0].to_plain_array_view::<f32>()?;
    let mut minx = 256usize; let mut miny = 144usize; let mut maxx = 0usize; let mut maxy = 0usize;
    for y in 0..144 { for x in 0..256 {
        if mv[[0, 0, y, x]] >= 0.5 {
            minx = minx.min(x); maxx = maxx.max(x); miny = miny.min(y); maxy = maxy.max(y);
        }
    }}
    println!("person bbox (256x144): x {minx}-{maxx}, y {miny}-{maxy}");

    // ---- mesh on a 256x256 face crop, for both normalizations ----
    let mesh = load_runnable("assets/face_landmarks_478.onnx")?;
    // detector best box, center-format [y, x, h, w] normalized per axis
    let det_row = best_row.clone().unwrap_or(vec![0.43f32, 0.32, 0.61, 0.49]);
    let bcx = det_row[1] * w as f32;
    let bcy = det_row[0] * h as f32;
    let bw = det_row[3] * w as f32;
    let bh = det_row[2] * h as f32;
    let dr = best_row.clone().unwrap();
    // blazeface 6 keypoints: cols 4..16 as (x,y) pairs, normalized per axis
    let kp = |i: usize| (dr[4 + 2 * i] * w as f32, dr[5 + 2 * i] * h as f32);
    let (e0, e1) = (kp(0), kp(1));
    let eye_mid = ((e0.0 + e1.0) / 2.0, (e0.1 + e1.1) / 2.0);
    let eye_dist = ((e0.0 - e1.0).powi(2) + (e0.1 - e1.1).powi(2)).sqrt();
    println!("kps: e0={e0:?} e1={e1:?} eye_mid={eye_mid:?} eye_dist={eye_dist:.1}");
    // corner-format hypothesis: row = [y1, x1, y2, x2] normalized per axis
    let cx1 = det_row[1] * w as f32;
    let cy1 = det_row[0] * h as f32;
    let cx2 = det_row[3] * w as f32;
    let cy2 = det_row[2] * h as f32;
    let ccx = (cx1 + cx2) / 2.0;
    let ccy = (cy1 + cy2) / 2.0;
    let cw = (cx2 - cx1).abs();
    let ch = (cy2 - cy1).abs();
    let rules: [(&str, f32, f32); 4] = [
        ("corner*1.1", ccx, ccy),
        ("corner*1.3", ccx, ccy),
        ("corner*1.6", ccx, ccy),
        ("corner*0.42off", bcx + 0.22 * bw, bcy + 0.08 * bh),
    ];
    let sides = [cw.max(ch) * 1.1, cw.max(ch) * 1.3, cw.max(ch) * 1.6, bw.max(bh) * 0.42];
    for ((label, ccx, ccy), side_f) in rules.iter().zip(sides.iter()) {
     for (nlabel, norm, swap) in [("rgb01", 0.0f32, false)] {
        let side = *side_f as i32;
        let x = ((ccx - side as f32 / 2.0).max(0.0)) as i32;
        let y = ((ccy - side as f32 / 2.0).max(0.0)) as i32;
        let rw = (side as usize).min(w - x as usize) as i32;
        let rh = (side as usize).min(h - y as usize) as i32;
        let roi = opencv::core::Mat::roi(&img, opencv::core::Rect::new(x, y, rw, rh))?;
        let mut crop = opencv::core::Mat::default();
        opencv::imgproc::resize(&roi, &mut crop, opencv::core::Size::new(256, 256), 0.0, 0.0, opencv::imgproc::INTER_LINEAR)?;
        let mut crop_rgb = opencv::core::Mat::default();
        opencv::imgproc::cvt_color(&crop, &mut crop_rgb, opencv::imgproc::COLOR_BGR2RGB, 0, opencv::core::AlgorithmHint::ALGO_HINT_DEFAULT)?;
        let cb = crop_rgb.data_bytes()?;
        let mut nhwc = vec![0f32; 256 * 256 * 3];
        for p in 0..256 * 256 {
            for c in 0..3 {
                let cc = if swap { 2 - c } else { c };
                let raw = cb[p * 3 + cc] as f32;
                nhwc[p * 3 + c] = match norm {
                    1.0 => raw / 255.0 * 2.0 - 1.0,
                    2.0 => raw,
                    _ => raw / 255.0,
                };
            }
        }
        let input = Tensor::from(tract_ndarray::Array4::from_shape_vec((1, 256, 256, 3), nhwc)?);
        let outs = mesh.run(tvec!(input.into_tvalue()))?;
        let lm = outs[0].to_plain_array_view::<f32>()?;
        let presence = outs[1].to_plain_array_view::<f32>()?;
        let p = presence[[0, 0, 0, 0]];
        println!("mesh rule={label} [{nlabel}] presence logit {p:.2} (sigmoid {:.3})", 1.0 / (1.0 + (-p).exp()));
        let idx = [33usize, 133, 159, 145, 468, 362, 263, 386, 374, 473];
        for &i in &idx {
            let x = lm[[0, 0, 0, i * 3]];
            let y = lm[[0, 0, 0, i * 3 + 1]];
            println!("  lm[{i}] = ({x:.1}, {y:.1})");
        }
     }
    }
    Ok(())
}
