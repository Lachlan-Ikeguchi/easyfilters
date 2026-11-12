//! easyfilters — milestone 3: passthrough from the real camera to the
//! v4l2loopback virtual camera, with the ONNX person-segmentation mask used
//! to composite real filters into the feed (--mode overlay|mask|blur|replace).

mod composite;
mod frame;
mod segmentation;
mod v4l2;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use opencv::prelude::*;
use tracing::{info, warn};
use v4l2r::ioctl::Capabilities;
use v4l2r::{bindings, PixelFormat, QueueType};

use segmentation::{Segmenter, MODEL_H, MODEL_W};
use v4l2::Device;

const OUTPUT_LABEL: &str = "EasyFilters Virtual Camera";
const DEFAULT_MODEL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/selfie_segmentation_landscape.onnx"
);
const JPEG_QUALITY: i32 = 80;

/// What gets written to the virtual camera.
#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    /// Milestone 1 behavior: copy the input MJPG frame verbatim.
    Passthrough,
    /// Frame with the segmented person kept, background dimmed/tinted.
    Overlay,
    /// Pure grayscale person mask.
    Mask,
    /// Person kept sharp, background Gaussian-blurred (--blur-strength).
    Blur,
    /// Person kept sharp, background replaced (--background image or a
    /// solid #003200 default).
    Replace,
}

#[derive(Default)]
struct StageTiming {
    decode: Duration,
    infer: Duration,
    composite: Duration,
    encode: Duration,
}

#[derive(Parser)]
#[command(
    name = "easyfilters",
    about = "Virtual camera with background filters (milestone 3: blur/replace compositing)"
)]
struct Args {
    /// Input camera device (default: auto-detect first uvcvideo capture node)
    #[arg(long)]
    input: Option<PathBuf>,
    /// Output virtual camera device (default: auto-discover by card label)
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Pixel format fourcc, e.g. MJPG or YUYV (default: auto, prefer MJPG)
    #[arg(long)]
    format: Option<String>,
    /// Number of MMAP buffers per queue
    #[arg(long, default_value_t = 4)]
    buffers: u32,
    /// List devices and their formats, then exit
    #[arg(long)]
    list: bool,
    /// What to write to the virtual camera
    #[arg(long, value_enum, default_value_t = Mode::Passthrough)]
    mode: Mode,
    /// ONNX segmentation model (default: bundled assets/ model, or $EASYFILTERS_MODEL)
    #[arg(long)]
    model: Option<PathBuf>,
    /// Gaussian blur sigma for --mode blur, expressed at 720p (scaled with the actual height)
    #[arg(long, default_value_t = 15.0)]
    blur_strength: f64,
    /// Background image for --mode replace (default: solid #003200)
    #[arg(long)]
    background: Option<PathBuf>,
    /// EMA alpha for mask temporal smoothing, 0..1 (higher = smoother, more lag)
    #[arg(long, default_value_t = 0.6)]
    mask_smoothing: f32,
    /// Decode one image, run segmentation, write mask.png/overlay.png/blur.png/replace.png next to it, exit
    #[arg(long)]
    self_test: Option<PathBuf>,
}

fn model_path(args: &Args) -> PathBuf {
    args.model
        .clone()
        .or_else(|| std::env::var("EASYFILTERS_MODEL").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MODEL))
}

/// Background for --mode replace: the --background image resized to the
/// frame, or a solid #003200 when no image is given.
fn load_background(args: &Args, w: u32, h: u32) -> Result<opencv::core::Mat> {
    match &args.background {
        Some(p) => {
            let img = frame::load_image(p)?;
            frame::resize(&img, w, h)
        }
        None => composite::default_background(w, h),
    }
}

/// Decode `path` (JPEG), run the segmenter, write mask.png / overlay.png
/// next to it and print the mask coverage. Exits without touching cameras.
fn self_test(path: &Path, args: &Args) -> Result<()> {
    let mp = model_path(args);
    let seg = Segmenter::load(&mp)
        .with_context(|| format!("load model {}", mp.display()))?;
    let jpeg = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let img = frame::decode_mjpg(&jpeg)?;
    let (w, h) = (img.cols() as u32, img.rows() as u32);
    info!(width = w, height = h, "decoded self-test image");

    let t = Instant::now();
    let small = frame::resize(&img, MODEL_W as u32, MODEL_H as u32)?;
    let rgb = frame::bgr_to_rgb(&small)?;
    let mask = seg.mask(&rgb, MODEL_W, MODEL_H)?;
    let infer = t.elapsed();

    let coverage = mask.iter().filter(|&&v| v > 0).count() as f64 / mask.len() as f64;
    println!(
        "mask coverage: {:.1}%  (infer {:.1} ms)",
        coverage * 100.0,
        infer.as_secs_f64() * 1000.0
    );

    let mask_small = frame::mask_mat(&mask, MODEL_W as u32, MODEL_H as u32)?;
    let mask_full = frame::upscale_mask(&mask_small, w, h)?;
    let overlay = frame::render_overlay(&img, &mask_full)?;
    let mask_img = frame::render_mask(&mask_full)?;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mask_path = dir.join("mask.png");
    let overlay_path = dir.join("overlay.png");
    frame::write_image(&mask_path, &mask_img)?;
    frame::write_image(&overlay_path, &overlay)?;
    println!("wrote {} and {}", mask_path.display(), overlay_path.display());

    // Milestone 3: blur / replace artifacts. A single frame has no history,
    // so use alpha = 1.0 to settle the EMA state on this frame's mask.
    let mut smoother = composite::MaskSmoother::new(MODEL_W * MODEL_H, 1.0);
    let soft = smoother.update(&mask);
    let mask_full_f = composite::feather_mask(soft, MODEL_W, MODEL_H, w, h)?;
    let sigma = args.blur_strength * h as f64 / 720.0;
    let blurred = composite::blur_and_composite(&img, &mask_full_f, sigma)?;
    let bg = load_background(args, w, h)?;
    let replaced = composite::replace_and_composite(&img, &bg, &mask_full_f)?;
    let blur_path = dir.join("blur.png");
    let replace_path = dir.join("replace.png");
    frame::write_image(&blur_path, &blurred)?;
    frame::write_image(&replace_path, &replaced)?;
    println!("wrote {} and {}", blur_path.display(), replace_path.display());
    Ok(())
}

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_sig: std::os::raw::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn install_signal_handlers() {
    use v4l2r::nix::libc;
    // SAFETY: process-global handler installation, done once at startup.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_signal as libc::sighandler_t;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

fn now_timeval() -> bindings::timeval {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    bindings::timeval {
        tv_sec: d.as_secs() as i64,
        tv_usec: d.subsec_micros() as i64,
    }
}

/// Open every /dev/videoN that responds to QUERYCAP.
fn scan_devices() -> Result<Vec<Device>> {
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

fn default_input(devs: &[Device]) -> Option<PathBuf> {
    devs.iter()
        .find(|d| {
            d.device_caps().contains(Capabilities::VIDEO_CAPTURE) && d.driver() == "uvcvideo"
        })
        .map(|d| d.path().to_path_buf())
}

fn find_output(devs: &[Device], label: &str) -> Option<PathBuf> {
    devs.iter()
        .find(|d| {
            d.device_caps().contains(Capabilities::VIDEO_OUTPUT) && d.card() == label
        })
        .map(|d| d.path().to_path_buf())
}

fn print_list(devs: &[Device]) {
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

fn parse_fourcc(s: &str) -> Result<PixelFormat> {
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
fn negotiate(dev: &Device, args: &Args) -> Result<(PixelFormat, u32, u32, u32, u32)> {
    let fmts = dev.enum_formats(QueueType::VideoCapture);
    let pf = match &args.format {
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
            let dw = sw.abs_diff(args.width) as u64;
            let dh = sh.abs_diff(args.height) as u64;
            dw * dw + dh * dh
        })
        .copied()
        .unwrap();

    let intervals = dev.frame_intervals(pf, w, h);
    let (num, den) = intervals
        .iter()
        .min_by_key(|(n, d)| {
            let fps = *d as f64 / *n as f64;
            (fps - args.fps as f64).abs() as u64
        })
        .copied()
        .ok_or_else(|| anyhow!("no frame intervals for {} {}x{}", pf, w, h))?;

    Ok((pf, w, h, num, den))
}

fn run(args: Args) -> Result<()> {
    let devs = scan_devices()?;
    if args.list {
        print_list(&devs);
        return Ok(());
    }
    if let Some(p) = &args.self_test {
        return self_test(p, &args);
    }

    let input_path = match &args.input {
        Some(p) => p.clone(),
        None => default_input(&devs)
            .ok_or_else(|| anyhow!("no uvcvideo capture device found; pass --input"))?,
    };
    let output_path = match &args.output {
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

    let (pf, w, h, num, den) = negotiate(&in_dev, &args)?;
    let in_fmt = in_dev.set_format(QueueType::VideoCapture, w, h, pf, 0)?;
    in_dev.set_fps(QueueType::VideoCapture, num, den)?;
    // Same fourcc/resolution on the loopback output queue. For compressed
    // formats the loopback needs an explicit sizeimage to size its buffers.
    let sizeimage = w * h * 2;
    let out_fmt = out_dev.set_format(QueueType::VideoOutput, w, h, pf, sizeimage)?;
    info!(
        input = %input_path.display(),
        output = %output_path.display(),
        format = %pf,
        width = in_fmt.width, height = in_fmt.height,
        sizeimage = out_fmt.plane_fmt[0].sizeimage,
        interval = format!("{}/{}", num, den),
        "negotiated format"
    );

    let seg = match args.mode {
        Mode::Passthrough => None,
        _ => {
            let mp = model_path(&args);
            let s = Segmenter::load(&mp)
                .with_context(|| format!("load model {}", mp.display()))?;
            info!(model = %mp.display(), "segmenter loaded");
            Some(s)
        }
    };

    let bg = match args.mode {
        Mode::Replace => {
            let b = load_background(&args, in_fmt.width, in_fmt.height)?;
            info!(
                background = %args
                    .background
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "#003200".to_string()),
                "background ready"
            );
            Some(b)
        }
        _ => None,
    };
    let mut smoother = composite::MaskSmoother::new(MODEL_W * MODEL_H, args.mask_smoothing);
    // --blur-strength is expressed at 720p; scale with the actual frame height.
    let sigma = args.blur_strength * in_fmt.height as f64 / 720.0;

    let in_q = in_dev.queue(QueueType::VideoCapture, args.buffers)?;
    let mut out_q = out_dev.queue(QueueType::VideoOutput, args.buffers)?;
    info!(buffers = in_q.buffers.len(), "queues allocated");

    in_q.stream_on()?;
    out_q.stream_on()?;
    for i in 0..in_q.buffers.len() as u32 {
        in_q.qbuf(i, 0, now_timeval())?;
    }

    let mut out_inflight = vec![false; out_q.buffers.len()];
    let mut frames = 0u64;
    let mut drops = 0u64;
    let mut last = Instant::now();
    let mut stage = StageTiming::default();

    let n_out = out_q.buffers.len();
    while RUNNING.load(Ordering::SeqCst) {
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
            match out_inflight.iter().position(|&f| !f) {
                Some(oidx) => {
                    let src = &in_q.buffers[d.index as usize].mapping.as_ref()
                        [..d.bytesused as usize];
                    let (n, timing) = match (&seg, args.mode) {
                        (None, Mode::Passthrough) => {
                            let dst = out_q.buffers[oidx].mapping.as_mut();
                            let n = src.len().min(dst.len());
                            dst[..n].copy_from_slice(&src[..n]);
                            (n, StageTiming::default())
                        }
                        (Some(seg), mode) => {
                            let t = Instant::now();
                            let img = match frame::decode_mjpg(src) {
                                Ok(img) => img,
                                Err(e) => {
                                    // A corrupt frame (e.g. right after a
                                    // stream restart) must not kill the app.
                                    warn!(error = %e, "dropping undecodable frame");
                                    drops += 1;
                                    in_q.qbuf(d.index, 0, now_timeval())?;
                                    continue;
                                }
                            };
                            let decode = t.elapsed();
                            let t = Instant::now();
                            let small =
                                frame::resize(&img, MODEL_W as u32, MODEL_H as u32)?;
                            let rgb = frame::bgr_to_rgb(&small)?;
                            let mask = seg.mask(&rgb, MODEL_W, MODEL_H)?;
                            let infer = t.elapsed();
                            let t = Instant::now();
                            let out_img = match mode {
                                Mode::Overlay => {
                                    let mask_small = frame::mask_mat(
                                        &mask,
                                        MODEL_W as u32,
                                        MODEL_H as u32,
                                    )?;
                                    let mask_full = frame::upscale_mask(
                                        &mask_small,
                                        in_fmt.width,
                                        in_fmt.height,
                                    )?;
                                    frame::render_overlay(&img, &mask_full)?
                                }
                                Mode::Mask => {
                                    let mask_small = frame::mask_mat(
                                        &mask,
                                        MODEL_W as u32,
                                        MODEL_H as u32,
                                    )?;
                                    let mask_full = frame::upscale_mask(
                                        &mask_small,
                                        in_fmt.width,
                                        in_fmt.height,
                                    )?;
                                    frame::render_mask(&mask_full)?
                                }
                                Mode::Blur | Mode::Replace => {
                                    let soft = smoother.update(&mask);
                                    let mask_full = composite::feather_mask(
                                        soft,
                                        MODEL_W,
                                        MODEL_H,
                                        in_fmt.width,
                                        in_fmt.height,
                                    )?;
                                    match mode {
                                        Mode::Blur => composite::blur_and_composite(
                                            &img,
                                            &mask_full,
                                            sigma,
                                        )?,
                                        Mode::Replace => composite::replace_and_composite(
                                            &img,
                                            bg.as_ref().unwrap(),
                                            &mask_full,
                                        )?,
                                        _ => unreachable!(),
                                    }
                                }
                                Mode::Passthrough => unreachable!(),
                            };
                            let composite = t.elapsed();
                            let t = Instant::now();
                            let jpeg = frame::encode_jpeg(&out_img, JPEG_QUALITY)?;
                            let dst = out_q.buffers[oidx].mapping.as_mut();
                            let n = jpeg.len().min(dst.len());
                            dst[..n].copy_from_slice(&jpeg[..n]);
                            let encode = t.elapsed();
                            (n, StageTiming { decode, infer, composite, encode })
                        }
                        (None, _) => unreachable!(),
                    };
                    out_q.qbuf(oidx as u32, n as u32, now_timeval())?;
                    out_inflight[oidx] = true;
                    frames += 1;
                    stage = timing;
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
            info!(
                frames,
                drops,
                fps,
                decode_ms = stage.decode.as_secs_f64() * 1000.0,
                infer_ms = stage.infer.as_secs_f64() * 1000.0,
                composite_ms = stage.composite.as_secs_f64() * 1000.0,
                encode_ms = stage.encode.as_secs_f64() * 1000.0,
                "stats"
            );
            frames = 0;
            drops = 0;
            last = Instant::now();
        }
    }

    info!("shutting down");
    in_q.stream_off()?;
    out_q.stream_off()?;
    in_q.free()?;
    out_q.free()?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    install_signal_handlers();
    run(Args::parse())
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
    fn model_path_prefers_cli_arg() {
        let args = Args::try_parse_from(["easyfilters", "--model", "/tmp/custom.onnx"]).unwrap();
        assert_eq!(model_path(&args), PathBuf::from("/tmp/custom.onnx"));
    }

    #[test]
    fn model_path_falls_back_to_bundled_default() {
        // The env-var branch is not exercised here (mutating the process env
        // in tests is unsafe in edition 2024 and racy across test threads).
        if std::env::var_os("EASYFILTERS_MODEL").is_some() {
            return;
        }
        let args = Args::try_parse_from(["easyfilters"]).unwrap();
        assert_eq!(model_path(&args), PathBuf::from(DEFAULT_MODEL));
    }

    #[test]
    fn now_timeval_is_sane() {
        let tv = now_timeval();
        assert!(tv.tv_sec > 1_700_000_000); // after 2023-11-14
        assert!((0..1_000_000).contains(&tv.tv_usec));
    }
}
