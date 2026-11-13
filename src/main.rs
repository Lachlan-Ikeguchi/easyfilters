//! easyfilters — milestone 4: GUI + tray + config around the milestone 1-3
//! capture/segment/composite pipeline. Without a display (or with --no-gui)
//! it runs the pipeline headless with the previous CLI behavior.

mod composite;
mod config;
mod frame;
mod gui;
mod pipeline;
mod segmentation;
mod settings;
mod tray;
mod v4l2;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use opencv::prelude::*;
use tracing::{error, info, warn};

use config::Config;
use segmentation::{Segmenter, MODEL_H, MODEL_W};
use settings::{DeviceParams, Mode, Settings, Shared};

/// Process-global shutdown flag. The signal handler cannot capture, so it
/// writes this static; `Shared::running()` reads it.
pub(crate) static RUNNING: AtomicBool = AtomicBool::new(true);

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

#[derive(Parser)]
#[command(
    name = "easyfilters",
    about = "Virtual camera with background filters (milestone 4: GUI + tray + config; --no-gui for headless)"
)]
struct Args {
    /// Input camera device (default: auto-detect first uvcvideo capture node)
    #[arg(long)]
    input: Option<PathBuf>,
    /// Output virtual camera device (default: auto-discover by card label)
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    width: Option<u32>,
    #[arg(long)]
    height: Option<u32>,
    #[arg(long)]
    fps: Option<u32>,
    /// Pixel format fourcc, e.g. MJPG or YUYV (default: auto, prefer MJPG)
    #[arg(long)]
    format: Option<String>,
    /// Number of MMAP buffers per queue
    #[arg(long)]
    buffers: Option<u32>,
    /// List devices and their formats, then exit
    #[arg(long)]
    list: bool,
    /// What to write to the virtual camera (default: replace with the solid
    /// #003200 background)
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// ONNX segmentation model (default: bundled assets/ model, or $EASYFILTERS_MODEL)
    #[arg(long)]
    model: Option<PathBuf>,
    /// Gaussian blur sigma for --mode blur, expressed at 720p (scaled with the actual height)
    #[arg(long)]
    blur_strength: Option<f64>,
    /// Background image for --mode replace (default: solid #003200)
    #[arg(long)]
    background: Option<PathBuf>,
    /// EMA alpha for mask temporal smoothing, 0..1 (higher = smoother, more lag)
    #[arg(long)]
    mask_smoothing: Option<f32>,
    /// Decode one image, run segmentation, write mask.png/overlay.png/blur.png/replace.png next to it, exit
    #[arg(long)]
    self_test: Option<PathBuf>,
    /// Disable filtering (passthrough); overrides `enabled` in the config
    #[arg(long)]
    disable: bool,
    /// Run headless: no GUI, no tray (milestone 1-3 behavior)
    #[arg(long)]
    no_gui: bool,
    /// Config file path (default: $XDG_CONFIG_HOME/easyfilters/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
}

struct Resolved {
    settings: Settings,
    device: DeviceParams,
}

/// Effective configuration: CLI arg > config file > built-in default.
fn resolve(args: &Args, cfg: &Config) -> Resolved {
    let enabled = if args.disable { Some(false) } else { cfg.enabled };
    Resolved {
        settings: Settings {
            enabled: enabled.unwrap_or(true),
            mode: args.mode.or(cfg.mode).unwrap_or(Mode::Replace),
            blur_strength: args.blur_strength.or(cfg.blur_strength).unwrap_or(15.0),
            mask_smoothing: args.mask_smoothing.or(cfg.mask_smoothing).unwrap_or(0.6),
            background: args.background.clone().or_else(|| cfg.background.clone()),
        },
        device: DeviceParams {
            input: args.input.clone().or_else(|| cfg.input.clone()),
            output: args.output.clone().or_else(|| cfg.output.clone()),
            width: args.width.or(cfg.width).unwrap_or(1280),
            height: args.height.or(cfg.height).unwrap_or(720),
            fps: args.fps.or(cfg.fps).unwrap_or(30),
            format: args.format.clone().or_else(|| cfg.format.clone()),
            buffers: args.buffers.or(cfg.buffers).unwrap_or(4),
            model: args.model.clone().or_else(|| cfg.model.clone()),
        },
    }
}

fn display_available() -> bool {
    let non_empty = |v: Option<std::ffi::OsString>| v.map(|s| !s.is_empty()).unwrap_or(false);
    non_empty(std::env::var_os("DISPLAY")) || non_empty(std::env::var_os("WAYLAND_DISPLAY"))
}

/// Decode `path` (JPEG), run the segmenter, write mask.png / overlay.png /
/// blur.png / replace.png next to it and print the mask coverage. Exits
/// without touching cameras.
fn self_test(path: &Path, resolved: &Resolved) -> Result<()> {
    let mp = pipeline::resolve_model(&resolved.device.model);
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

    // Blur / replace artifacts. A single frame has no history, so use
    // alpha = 1.0 to settle the EMA state on this frame's mask.
    let mut smoother = composite::MaskSmoother::new(MODEL_W * MODEL_H, 1.0);
    let soft = smoother.update(&mask);
    let mask_full_f = composite::feather_mask(soft, MODEL_W, MODEL_H, w, h)?;
    let sigma = resolved.settings.blur_strength * h as f64 / 720.0;
    let blurred = composite::blur_and_composite(&img, &mask_full_f, sigma)?;
    let bg = pipeline::load_background(resolved.settings.background.as_deref(), w, h)?;
    let replaced = composite::replace_and_composite(&img, &bg, &mask_full_f)?;
    let blur_path = dir.join("blur.png");
    let replace_path = dir.join("replace.png");
    frame::write_image(&blur_path, &blurred)?;
    frame::write_image(&replace_path, &replaced)?;
    println!("wrote {} and {}", blur_path.display(), replace_path.display());
    Ok(())
}

fn run(args: Args) -> Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                error = %e,
                path = %config_path.display(),
                "could not load config; using defaults"
            );
            Config::default()
        }
    };
    let resolved = resolve(&args, &cfg);
    info!(config = %config_path.display(), mode = %resolved.settings.mode, enabled = resolved.settings.enabled, "configuration resolved");

    if args.list {
        let devs = pipeline::scan_devices()?;
        pipeline::print_list(&devs);
        return Ok(());
    }
    if let Some(p) = &args.self_test {
        return self_test(p, &resolved);
    }

    let shared = Arc::new(Shared::new(resolved.settings, resolved.device));
    let want_gui = !args.no_gui && display_available();
    if !want_gui && !args.no_gui {
        info!("no DISPLAY/WAYLAND_DISPLAY set; running headless (--no-gui)");
    }

    if want_gui {
        shared.preview_on.store(true, Ordering::Relaxed);
        let pipe_shared = shared.clone();
        let pipe = thread::spawn(move || {
            if let Err(e) = pipeline::run(pipe_shared.clone()) {
                error!(error = %e, "pipeline failed");
                pipe_shared.stop();
            }
        });
        tray::spawn(shared.clone(), config_path.clone());
        let gui_result = gui::run(shared.clone(), config_path);
        // Window closed or tray quit: stop the pipeline and wait for it.
        shared.stop();
        let _ = pipe.join();
        gui_result.map_err(|e| anyhow::anyhow!("gui exited with error: {e}"))
    } else {
        pipeline::run(shared)
    }
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
    fn resolve_cli_beats_config_beats_default() {
        let args = Args::try_parse_from(["easyfilters", "--mode", "blur", "--width", "640"]).unwrap();
        let cfg = Config {
            mode: Some(Mode::Mask),
            width: Some(1920),
            fps: Some(15),
            ..Default::default()
        };
        let r = resolve(&args, &cfg);
        assert_eq!(r.settings.mode, Mode::Blur); // CLI wins over config
        assert_eq!(r.device.width, 640); // CLI wins over config
        assert_eq!(r.device.fps, 15); // config wins over default
        assert_eq!(r.device.height, 720); // built-in default
        assert!(r.settings.enabled); // default
    }

    #[test]
    fn resolve_disable_flag_overrides_config_enabled() {
        let cfg = Config {
            enabled: Some(true),
            ..Default::default()
        };
        let args = Args::try_parse_from(["easyfilters", "--disable"]).unwrap();
        assert!(!resolve(&args, &cfg).settings.enabled);
        let args = Args::try_parse_from(["easyfilters"]).unwrap();
        assert!(resolve(&args, &cfg).settings.enabled);
    }

    #[test]
    fn resolve_config_background_used_without_cli() {
        let cfg = Config {
            background: Some(PathBuf::from("/tmp/bg.png")),
            ..Default::default()
        };
        let args = Args::try_parse_from(["easyfilters"]).unwrap();
        assert_eq!(
            resolve(&args, &cfg).settings.background,
            Some(PathBuf::from("/tmp/bg.png"))
        );
    }
}
