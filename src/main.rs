//! easyfilters — milestone 1: unfiltered passthrough from the real camera to
//! the v4l2loopback virtual camera (compressed MJPG copied verbatim).

mod v4l2;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use tracing::info;
use v4l2r::ioctl::Capabilities;
use v4l2r::{bindings, PixelFormat, QueueType};

use v4l2::Device;

const OUTPUT_LABEL: &str = "EasyFilters Virtual Camera";

#[derive(Parser)]
#[command(
    name = "easyfilters",
    about = "Virtual camera with background filters (milestone 1: unfiltered passthrough)"
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
                    let n = {
                        let src = &in_q.buffers[d.index as usize].mapping.as_ref()
                            [..d.bytesused as usize];
                        let dst = out_q.buffers[oidx].mapping.as_mut();
                        let n = src.len().min(dst.len());
                        dst[..n].copy_from_slice(&src[..n]);
                        n
                    };
                    out_q.qbuf(oidx as u32, n as u32, now_timeval())?;
                    out_inflight[oidx] = true;
                    frames += 1;
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
            info!(frames, drops, fps, "stats");
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
