//! The capture -> segment -> composite -> loopback pipeline (milestones 1-3),
//! refactored in milestone 4 to run on its own thread, driven live by the
//! `Shared` settings the GUI/tray write. The per-frame hot path only takes an
//! uncontended `RwLock` read plus a few atomic loads; anything expensive
//! (model load, background image load) happens once, lazily, or on a
//! generation bump.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use opencv::core::Mat;
use opencv::prelude::*;
use tracing::{info, warn};
use v4l2r::ioctl::Capabilities;
use v4l2r::{bindings, PixelFormat, QueueType};

use crate::gaze::GazeCorrector;
use crate::segmentation::{Segmenter, MODEL_H, MODEL_W};
use crate::settings::{DeviceParams, Mode, Shared, Stats};
use crate::v4l2::{Device, Queue};
use crate::pwsource::PwSource;
use crate::{composite, frame};

pub(crate) const OUTPUT_LABEL: &str = "EasyFilters Virtual Camera";
pub(crate) const DEFAULT_MODEL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/selfie_segmentation_landscape.onnx"
);
pub(crate) const DEFAULT_FACE_MODEL: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/assets/face_landmarks_478.onnx");
pub(crate) const DEFAULT_FACE_DETECTOR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/assets/blaze_face_short_range.onnx");
const JPEG_QUALITY: i32 = 80;

/// CLI arg > $EASYFILTERS_MODEL > bundled model.
pub(crate) fn resolve_model(model: &Option<PathBuf>) -> PathBuf {
    model
        .clone()
        .or_else(|| std::env::var("EASYFILTERS_MODEL").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MODEL))
}

/// CLI arg > $EASYFILTERS_FACE_MODEL > bundled face-mesh model.
pub(crate) fn resolve_face_model(model: &Option<PathBuf>) -> PathBuf {
    model
        .clone()
        .or_else(|| std::env::var("EASYFILTERS_FACE_MODEL").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FACE_MODEL))
}

/// $EASYFILTERS_FACE_DETECTOR > bundled BlazeFace detector (no CLI flag; the
/// detector always ships bundled, the env var exists for the nix package).
pub(crate) fn resolve_face_detector() -> PathBuf {
    std::env::var("EASYFILTERS_FACE_DETECTOR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FACE_DETECTOR))
}

pub(crate) fn now_timeval() -> bindings::timeval {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    bindings::timeval {
        tv_sec: d.as_secs() as i64,
        tv_usec: d.subsec_micros() as i64,
    }
}

/// Open every /dev/videoN that responds to QUERYCAP.
pub(crate) fn scan_devices() -> Result<Vec<Device>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/sys/class/video4linux").context("read /sys/class/video4linux")? {
        let entry = entry?;
        let node = PathBuf::from(format!("/dev/{}", entry.file_name().to_string_lossy()));
        if let Ok(dev) = Device::open(&node, true) {
            out.push(dev);
        }
    }
    out.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(out)
}

pub(crate) fn default_input(devs: &[Device]) -> Option<PathBuf> {
    devs.iter()
        .find(|d| {
            d.device_caps().contains(Capabilities::VIDEO_CAPTURE) && d.driver() == "uvcvideo"
        })
        .map(|d| d.path().to_path_buf())
}

pub(crate) fn find_output(devs: &[Device], label: &str) -> Option<PathBuf> {
    devs.iter()
        .find(|d| {
            d.device_caps().contains(Capabilities::VIDEO_OUTPUT) && d.card() == label
        })
        .map(|d| d.path().to_path_buf())
}

pub(crate) fn print_list(devs: &[Device]) {
    for dev in devs {
        let caps = dev.device_caps();
        println!(
            "{}  driver={} card=\"{}\" caps={:?}",
            dev.path().display(),
            dev.driver(),
            dev.card(),
            caps
        );
        for (queue, supported) in [
            (QueueType::VideoCapture, caps.contains(Capabilities::VIDEO_CAPTURE)),
            (QueueType::VideoOutput, caps.contains(Capabilities::VIDEO_OUTPUT)),
        ] {
            if !supported {
                continue;
            }
            for desc in dev.enum_formats(queue) {
                println!("  {:?}: {}", queue, desc);
                for (w, h) in dev.frame_sizes(desc.pixelformat) {
                    let intervals = dev.frame_intervals(desc.pixelformat, w, h);
                    let iv: Vec<String> = intervals
                        .iter()
                        .map(|(n, d)| format!("{:.3}s ({:.3} fps)", *n as f64 / *d as f64, *d as f64 / *n as f64))
                        .collect();
                    println!("    {}x{}: {}", w, h, iv.join(", "));
                }
            }
        }
    }
}

pub(crate) fn parse_fourcc(s: &str) -> Result<PixelFormat> {
    let b = s.as_bytes();
    if b.len() != 4 {
        bail!("--format must be a 4-character fourcc, got {:?}", s);
    }
    let mut a = [0u8; 4];
    a.copy_from_slice(b);
    Ok(PixelFormat::from(&a))
}

/// Pick format, nearest size and closest frame interval. Returns
/// (pixelformat, width, height, interval numerator, interval denominator).
pub(crate) fn negotiate(dev: &Device, p: &DeviceParams) -> Result<(PixelFormat, u32, u32, u32, u32)> {
    let fmts = dev.enum_formats(QueueType::VideoCapture);
    let pf = match &p.format {
        Some(f) => parse_fourcc(f)?,
        None => [b"MJPG", b"YUYV"]
            .iter()
            .find_map(|cc| {
                let want = PixelFormat::from(*cc);
                fmts.iter().find(|d| d.pixelformat == want).map(|d| d.pixelformat)
            })
            .ok_or_else(|| anyhow!("camera supports neither MJPG nor YUYV"))?,
    };

    let sizes = dev.frame_sizes(pf);
    if sizes.is_empty() {
        bail!("no discrete frame sizes for {}", pf);
    }
    let (w, h) = sizes
        .iter()
        .min_by_key(|(sw, sh)| {
            let dw = sw.abs_diff(p.width) as u64;
            let dh = sh.abs_diff(p.height) as u64;
            dw * dw + dh * dh
        })
        .copied()
        .unwrap();

    let intervals = dev.frame_intervals(pf, w, h);
    let (num, den) = intervals
        .iter()
        .min_by_key(|(n, d)| {
            let fps = *d as f64 / *n as f64;
            (fps - p.fps as f64).abs() as u64
        })
        .copied()
        .ok_or_else(|| anyhow!("no frame intervals for {} {}x{}", pf, w, h))?;

    Ok((pf, w, h, num, den))
}

/// Background for replace mode: the configured image resized to the frame,
/// or a solid #003200 when none is set.
pub(crate) fn load_background(background: Option<&Path>, w: u32, h: u32) -> Result<Mat> {
    match background {
        Some(p) => {
            let img = frame::load_image(p)?;
            frame::resize(&img, w, h)
        }
        None => composite::default_background(w, h),
    }
}

/// BGR Mat -> (width, height, tightly packed RGBA8).
fn mat_to_rgba(img: &Mat) -> Result<(u32, u32, Vec<u8>)> {
    let mut rgba = Mat::default();
    opencv::imgproc::cvt_color(
        img,
        &mut rgba,
        opencv::imgproc::COLOR_BGR2RGBA,
        0,
        opencv::core::AlgorithmHint::ALGO_HINT_DEFAULT,
    )?;
    Ok((
        rgba.cols() as u32,
        rgba.rows() as u32,
        rgba.data_bytes()?.to_vec(),
    ))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Run the pipeline until `shared` is stopped. Restarts the stream whenever
/// `device_generation` moves (GUI changed camera/resolution); retries
/// transient stream failures with a 2 s backoff instead of dying.
pub fn run(shared: Arc<Shared>) -> Result<()> {
    while shared.running() {
        match run_stream(&shared) {
            Ok(()) => {
                if !shared.running() {
                    return Ok(());
                }
                info!("restarting stream with new device settings");
            }
            Err(e) => {
                if !shared.running() {
                    return Err(e);
                }
                // Milestone 6: a busy camera (held by another app) deserves a
                // targeted hint, not just the generic retry message.
                let chain = format!("{e:?}");
                if chain.contains("EBUSY") || chain.contains("Device or resource busy") {
                    warn!(
                        error = %e,
                        "device busy — the camera or loopback may be held by another app \
                         (Discord/Zoom/browser?); close it or pass --input/--output; retrying in 2 s"
                    );
                } else {
                    warn!(error = %e, "stream failed; retrying in 2 s");
                }
                for _ in 0..20 {
                    if !shared.running() {
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Ok(())
}

/// Open devices, negotiate, stream, and run the capture loop once. Always
/// tears the queues down before returning.
fn run_stream(shared: &Shared) -> Result<()> {
    let dev_params = shared.device.read().unwrap().clone();
    let devs = scan_devices()?;
    let input_path = match &dev_params.input {
        Some(p) => p.clone(),
        None => default_input(&devs)
            .ok_or_else(|| anyhow!("no uvcvideo capture device found; pass --input"))?,
    };
    let output_path = match &dev_params.output {
        Some(p) => p.clone(),
        None => find_output(&devs, OUTPUT_LABEL).ok_or_else(|| {
            anyhow!(
                "no virtual camera with card label \"{OUTPUT_LABEL}\" found; \
                 run once as root: sudo scripts/setup-virtual-camera.sh"
            )
        })?,
    };
    drop(devs); // close the discovery handles; we re-open the chosen nodes below

    let in_dev = Device::open(&input_path, true)?;
    let out_dev = Device::open(&output_path, true)?;
    if !in_dev.device_caps().contains(Capabilities::VIDEO_CAPTURE) {
        bail!("{} has no VIDEO_CAPTURE capability", input_path.display());
    }
    if !out_dev.device_caps().contains(Capabilities::VIDEO_OUTPUT) {
        bail!("{} has no VIDEO_OUTPUT capability", output_path.display());
    }

    let (pf, w, h, num, den) = negotiate(&in_dev, &dev_params)?;
    let in_fmt = in_dev.set_format(QueueType::VideoCapture, w, h, pf, 0)?;
    in_dev.set_fps(QueueType::VideoCapture, num, den)?;
    // Same fourcc/resolution on the loopback output queue. For compressed
    // formats the loopback needs an explicit sizeimage to size its buffers.
    let sizeimage = w * h * 2;
    let out_fmt = out_dev.set_format(QueueType::VideoOutput, w, h, pf, sizeimage)?;
    // Milestone 6: log requested vs negotiated explicitly — the camera may
    // not support the requested WxH/fps and negotiate() silently picks the
    // nearest. The GUI status line shows this via `Stats::negotiated`.
    let negotiated = Some((in_fmt.width, in_fmt.height, den as f64 / num as f64));
    info!(
        input = %input_path.display(),
        output = %output_path.display(),
        format = %pf,
        requested = format!("{}x{}@{}", dev_params.width, dev_params.height, dev_params.fps),
        width = in_fmt.width, height = in_fmt.height,
        fps = den as f64 / num as f64,
        sizeimage = out_fmt.plane_fmt[0].sizeimage,
        interval = format!("{}/{}", num, den),
        "negotiated format"
    );

    let in_q = in_dev.queue(QueueType::VideoCapture, dev_params.buffers)?;
    let mut out_q = out_dev.queue(QueueType::VideoOutput, dev_params.buffers)?;
    info!(buffers = in_q.buffers.len(), "queues allocated");

    in_q.stream_on()?;
    out_q.stream_on()?;
    for i in 0..in_q.buffers.len() as u32 {
        in_q.qbuf(i, 0, now_timeval())?;
    }

    // Milestone 7: native PipeWire camera node publishing the same filtered
    // feed (RGBA), so PipeWire-based apps can consume it without touching
    // the v4l2loopback device. Failure must not kill the loopback path.
    let pw_node = if shared.snapshot_settings().pipewire {
        match PwSource::new(w, h, num, den) {
            Ok(node) => Some(node),
            Err(e) => {
                warn!(error = %e, "pipewire source unavailable; continuing without it");
                None
            }
        }
    } else {
        None
    };

    let result = capture_loop(
        shared,
        &in_dev,
        &in_q,
        &mut out_q,
        pw_node.as_ref(),
        in_fmt.width,
        in_fmt.height,
        negotiated,
    );

    let _ = in_q.stream_off();
    let _ = out_q.stream_off();
    let _ = in_q.free();
    let _ = out_q.free();
    info!("stream stopped");
    result
}

/// Decode + filter one raw input frame according to the current shared
/// settings. Returns the exact bytes to write to the loopback (a verbatim
/// copy for passthrough, JPEG otherwise), the stage timings, and an optional
/// RGBA preview frame. `Ok(None)` = drop the frame (undecodable input).
#[allow(clippy::too_many_arguments)]
fn filter_frame(
    shared: &Shared,
    seg: &mut Option<Segmenter>,
    smoother: &mut composite::MaskSmoother,
    smoother_alpha: &mut f32,
    bg: &mut Option<Mat>,
    bg_loaded_gen: &mut u64,
    gaze: &mut Option<GazeCorrector>,
    src: &[u8],
    w: u32,
    h: u32,
    produce_rgba: bool,
) -> Result<Option<(Vec<u8>, Stats, Option<(u32, u32, Vec<u8>)>)>> {
    let s = shared.snapshot_settings();

    // Passthrough (filter disabled or mode=passthrough): byte-copy. Decode
    // only for the preview when a GUI is attached. With mirror on the
    // byte-copy cannot flip, so decode -> flip -> re-encode instead.
    if !s.enabled || s.mode == Mode::Passthrough {
        if s.mirror {
            let img = match frame::decode_mjpg(src) {
                Ok(img) => img,
                Err(e) => {
                    warn!(error = %e, "dropping undecodable frame");
                    return Ok(None);
                }
            };
            let flipped = frame::flip_horizontal(&img)?;
            let preview = if produce_rgba {
                mat_to_rgba(&flipped).ok()
            } else {
                None
            };
            let jpeg = frame::encode_jpeg(&flipped, JPEG_QUALITY)?;
            return Ok(Some((jpeg, Stats::default(), preview)));
        }
        let preview = if produce_rgba {
            frame::decode_mjpg(src).ok().and_then(|img| mat_to_rgba(&img).ok())
        } else {
            None
        };
        return Ok(Some((src.to_vec(), Stats::default(), preview)));
    }

    let t = Instant::now();
    let img = match frame::decode_mjpg(src) {
        Ok(img) => img,
        Err(e) => {
            // A corrupt frame (e.g. right after a stream restart) must not
            // kill the app — drop-frame policy.
            warn!(error = %e, "dropping undecodable frame");
            return Ok(None);
        }
    };
    let decode = t.elapsed();

    // The segmenter loads lazily on the first filtered frame so
    // passthrough-only runs never pay for it.
    if seg.is_none() {
        let mp = resolve_model(&shared.device.read().unwrap().model);
        let loaded = Segmenter::load(&mp)
            .with_context(|| format!("load model {}", mp.display()))?;
        info!(model = %mp.display(), "segmenter loaded");
        *seg = Some(loaded);
    }

    let t = Instant::now();
    let small = frame::resize(&img, MODEL_W as u32, MODEL_H as u32)?;
    let rgb = frame::bgr_to_rgb(&small)?;
    let mask = seg.as_mut().unwrap().mask(&rgb, MODEL_W, MODEL_H)?;
    let infer = t.elapsed();

    let t = Instant::now();
    let mask_small = frame::mask_mat(&mask, MODEL_W as u32, MODEL_H as u32)?;
    let hard_full = frame::upscale_mask(&mask_small, w, h)?;
    // mask mode outputs the mask itself — no face to correct, gaze skips it.
    let (mut out_img, gaze_mask) = match s.mode {
        Mode::Overlay => (frame::render_overlay(&img, &hard_full)?, Some(hard_full)),
        Mode::Mask => (frame::render_mask(&hard_full)?, None),
        Mode::Blur | Mode::Replace => {
            // Recreate the smoother when the alpha slider moved (state
            // resets; acceptable for a live control change).
            if *smoother_alpha != s.mask_smoothing {
                *smoother = composite::MaskSmoother::new(MODEL_W * MODEL_H, s.mask_smoothing);
                *smoother_alpha = s.mask_smoothing;
            }
            let soft = smoother.update(&mask);
            let mask_full = composite::feather_mask(soft, MODEL_W, MODEL_H, w, h)?;
            let out = match s.mode {
                Mode::Blur => {
                    // blur_strength is expressed at 720p; scale with height.
                    let sigma = s.blur_strength * h as f64 / 720.0;
                    composite::blur_and_composite(&img, &mask_full, sigma)?
                }
                Mode::Replace => {
                    let bg_gen = shared.bg_generation.load(Ordering::Relaxed);
                    if bg.is_none() || *bg_loaded_gen != bg_gen {
                        let path = shared.settings.read().unwrap().background.clone();
                        let loaded = match load_background(path.as_deref(), w, h) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!(error = %e, "background load failed; using #003200");
                                composite::default_background(w, h)?
                            }
                        };
                        info!(
                            background = %path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "#003200".to_string()),
                            "background ready"
                        );
                        *bg = Some(loaded);
                        *bg_loaded_gen = bg_gen;
                    }
                    composite::replace_and_composite(&img, bg.as_ref().unwrap(), &mask_full)?
                }
                _ => unreachable!(),
            };
            (out, Some(mask_full))
        }
        Mode::Passthrough => unreachable!(),
    };
    let composite_t = t.elapsed();

    // Gaze correction (milestone 5): after composite, before encode/preview.
    // Applies to overlay/blur/replace (mask mode has no face to correct).
    let t = Instant::now();
    let gaze_ms = if s.gaze_correction {
        if let Some(gm) = gaze_mask.as_ref() {
            if gaze.is_none() {
                let det = resolve_face_detector();
                let mesh = resolve_face_model(&shared.device.read().unwrap().face_model);
                let loaded = GazeCorrector::load(&det, &mesh).with_context(|| {
                    format!("load face models ({}, {})", det.display(), mesh.display())
                })?;
                info!(detector = %det.display(), mesh = %mesh.display(), "gaze models loaded");
                *gaze = Some(loaded);
            }
            gaze.as_mut()
                .unwrap()
                .process(&img, &mut out_img, gm, s.gaze_strength)?;
        }
        ms(t.elapsed())
    } else {
        0.0
    };

    // Mirror the whole feed: flip the final composited frame horizontally,
    // so the loopback, the PipeWire node and the GUI preview all see the
    // same mirrored image (a replaced background is mirrored too).
    if s.mirror {
        out_img = frame::flip_horizontal(&out_img)?;
    }

    let preview = if produce_rgba {
        Some(mat_to_rgba(&out_img)?)
    } else {
        None
    };

    let t = Instant::now();
    let jpeg = frame::encode_jpeg(&out_img, JPEG_QUALITY)?;
    let encode = t.elapsed();

    let stats = Stats {
        decode_ms: ms(decode),
        infer_ms: ms(infer),
        composite_ms: ms(composite_t),
        encode_ms: ms(encode),
        gaze_ms,
        ..Stats::default()
    };
    Ok(Some((jpeg, stats, preview)))
}

fn capture_loop(
    shared: &Shared,
    in_dev: &Device,
    in_q: &Queue,
    out_q: &mut Queue,
    pw: Option<&PwSource>,
    w: u32,
    h: u32,
    negotiated: Option<(u32, u32, f64)>,
) -> Result<()> {
    let dev_generation = shared.device_generation.load(Ordering::Relaxed);

    let mut seg: Option<Segmenter> = None;
    let mut smoother = composite::MaskSmoother::new(MODEL_W * MODEL_H, 0.6);
    let mut smoother_alpha = 0.6f32;
    let mut bg: Option<Mat> = None;
    let mut bg_loaded_gen: u64 = u64::MAX;
    let mut gaze: Option<GazeCorrector> = None;

    let mut out_inflight = vec![false; out_q.buffers.len()];
    let mut frames = 0u64;
    let mut drops = 0u64;
    let mut seq = 0u64;
    let mut last = Instant::now();
    let mut stage = Stats::default();

    let n_out = out_q.buffers.len();
    while shared.running() {
        // A device-settings change (camera/resolution/fps from the GUI)
        // restarts the whole stream.
        if shared.device_generation.load(Ordering::Relaxed) != dev_generation {
            info!("device settings changed; restarting stream");
            return Ok(());
        }

        // Wait for a frame on the input; the timeout also lets us notice
        // shutdown requests and reclaim finished output buffers.
        in_dev.poll(Duration::from_secs(1))?;

        // Reclaim output buffers the loopback has finished with. Bound the
        // number of dqbuf calls and only accept indices we have in flight:
        // v4l2loopback 0.15.x can rotate never-queued buffers through the
        // output queue's dqbuf, which would otherwise loop forever.
        for _ in 0..n_out {
            match out_q.try_dqbuf()? {
                Some(d) if d.index < n_out as u32 && out_inflight[d.index as usize] => {
                    out_inflight[d.index as usize] = false;
                }
                Some(_) => {}
                None => break,
            }
        }

        if let Some(d) = in_q.try_dqbuf()? {
            // RGBA copy for the GUI preview and/or the PipeWire node.
            let produce_rgba =
                shared.preview_on.load(Ordering::Relaxed) || pw.is_some();
            match out_inflight.iter().position(|&f| !f) {
                Some(oidx) => {
                    let src = &in_q.buffers[d.index as usize].mapping.as_ref()
                        [..d.bytesused as usize];
                    match filter_frame(
                        shared,
                        &mut seg,
                        &mut smoother,
                        &mut smoother_alpha,
                        &mut bg,
                        &mut bg_loaded_gen,
                        &mut gaze,
                        src,
                        w,
                        h,
                        produce_rgba,
                    )? {
                        Some((bytes, timing, preview)) => {
                            // Milestone 7: hand the RGBA frame to the
                            // PipeWire node before the preview consumes it.
                            if let Some(node) = pw {
                                if let Some((pv_w, pv_h, rgba)) = preview.as_ref() {
                                    node.push(*pv_w, *pv_h, rgba);
                                }
                            }
                            if let Some((pv_w, pv_h, rgba)) = preview {
                                seq += 1;
                                shared.publish_preview(pv_w, pv_h, rgba, seq);
                            }
                            let dst = out_q.buffers[oidx].mapping.as_mut();
                            let n = bytes.len().min(dst.len());
                            dst[..n].copy_from_slice(&bytes[..n]);
                            out_q.qbuf(oidx as u32, n as u32, now_timeval())?;
                            out_inflight[oidx] = true;
                            frames += 1;
                            stage = timing;
                        }
                        None => {
                            // Undecodable frame: drop-frame policy.
                            drops += 1;
                        }
                    }
                }
                None => {
                    // Drop-frame policy: no free output buffer, drop the frame.
                    drops += 1;
                }
            }
            in_q.qbuf(d.index, 0, now_timeval())?;
        }

        let elapsed = last.elapsed();
        if elapsed >= Duration::from_secs(2) {
            let fps = frames as f64 / elapsed.as_secs_f64();
            let st = Stats {
                frames,
                drops,
                fps,
                negotiated,
                ..stage
            };
            *shared.stats.lock().unwrap() = st;
            info!(
                frames,
                drops,
                fps,
                decode_ms = st.decode_ms,
                infer_ms = st.infer_ms,
                composite_ms = st.composite_ms,
                encode_ms = st.encode_ms,
                gaze_ms = st.gaze_ms,
                "stats"
            );
            frames = 0;
            drops = 0;
            last = Instant::now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fourcc_accepts_exactly_four_chars() {
        assert_eq!(parse_fourcc("MJPG").unwrap().to_string(), "MJPG");
        assert_eq!(parse_fourcc("YUYV").unwrap().to_string(), "YUYV");
    }

    #[test]
    fn parse_fourcc_rejects_wrong_length() {
        assert!(parse_fourcc("MJP").is_err());
        assert!(parse_fourcc("MJPGS").is_err());
        assert!(parse_fourcc("").is_err());
    }

    #[test]
    fn resolve_model_prefers_explicit_arg() {
        let args = Some(PathBuf::from("/tmp/custom.onnx"));
        assert_eq!(resolve_model(&args), PathBuf::from("/tmp/custom.onnx"));
    }

    #[test]
    fn resolve_model_falls_back_to_bundled_default() {
        // The env-var branch is not exercised here (mutating the process env
        // in tests is unsafe in edition 2024 and racy across test threads).
        if std::env::var_os("EASYFILTERS_MODEL").is_some() {
            return;
        }
        assert_eq!(resolve_model(&None), PathBuf::from(DEFAULT_MODEL));
    }

    #[test]
    fn resolve_face_models_prefer_explicit_arg() {
        let args = Some(PathBuf::from("/tmp/custom-face.onnx"));
        assert_eq!(resolve_face_model(&args), PathBuf::from("/tmp/custom-face.onnx"));
        if std::env::var_os("EASYFILTERS_FACE_MODEL").is_none() {
            assert_eq!(resolve_face_model(&None), PathBuf::from(DEFAULT_FACE_MODEL));
        }
        if std::env::var_os("EASYFILTERS_FACE_DETECTOR").is_none() {
            assert_eq!(resolve_face_detector(), PathBuf::from(DEFAULT_FACE_DETECTOR));
        }
    }

    #[test]
    fn now_timeval_is_sane() {
        let tv = now_timeval();
        assert!(tv.tv_sec > 1_700_000_000); // after 2023-11-14
        assert!((0..1_000_000).contains(&tv.tv_usec));
    }
}
