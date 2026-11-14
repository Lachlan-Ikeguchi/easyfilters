//! Milestone 8: recording (plan/milestone8.md, option A) — an ffmpeg
//! subprocess writes the filtered feed (raw BGR24 on stdin) plus the
//! PulseAudio default input (`-f pulse -i default`) into an .ogv
//! (Ogg/Theora/Vorbis). No new cargo dependencies; ffmpeg is a runtime
//! dependency (nix wrapper prefixes PATH with ffmpeg-full; override with
//! $EASYFILTERS_FFMPEG).
//!
//! Sync design (verified experimentally, see plan/milestone8.md findings):
//! ffmpeg's rawvideo demuxer stamps frames by *count* at the `-r` framerate
//! — `-use_wallclock_as_timestamps` is accepted but ignored there — so a
//! writer thread writes exactly one frame per negotiated interval, repeating
//! the latest frame when the pipeline dropped one. Count-based PTS then
//! equals wall clock by construction, matching the live pulse audio stream.
//!
//! Threading: the hot capture loop only stores the newest frame into a
//! latest-wins slot (a mutex + Arc swap, same pattern as the PipeWire node in
//! pwsource.rs); the writer thread owns the ffmpeg stdin pipe and paces the
//! writes. Recording can therefore never stall the loopback path.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use opencv::core::{Mat, Scalar, CV_8UC3};
use opencv::prelude::*;
use tracing::{info, warn};
use v4l2r::nix::libc;

use crate::frame;

/// Frames taller than this are downscaled in the writer thread before being
/// written — libtheora is single-threaded and cannot hold 720p30 on a busy
/// CPU; 540p keeps a comfortable margin (see plan/milestone8.md risk R2).
pub(crate) const RECORD_MAX_HEIGHT: u32 = 540;
/// Theora video quality 0-10, Vorbis audio quality 0-10.
const THEORA_Q: &str = "5";
const VORBIS_Q: &str = "4";
/// How long to wait for ffmpeg to finalize after SIGINT before SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Latest-wins frame slot between the capture loop and the writer thread.
type FrameSlot = Arc<Mutex<Option<Arc<[u8]>>>>;

pub struct Recorder {
    child: Child,
    slot: FrameSlot,
    running: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    frames: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
    path: PathBuf,
}

/// ffmpeg binary: $EASYFILTERS_FFMPEG > `ffmpeg` on PATH.
fn ffmpeg_binary() -> String {
    std::env::var("EASYFILTERS_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// The exact ffmpeg argv for a recording of `rw`x`rh` at the negotiated
/// v4l2 interval num/den (seconds per frame — ffmpeg's `-r` wants the
/// framerate, i.e. den/num). Pure function so tests can assert on it.
fn ffmpeg_args(rw: u32, rh: u32, den: u32, num: u32, audio: bool, path: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "-y",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgr24",
        "-s",
        &format!("{rw}x{rh}"),
        "-r",
        &format!("{den}/{num}"),
        "-i",
        "pipe:0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if audio {
        // The PulseAudio default input device (NULL device = server default).
        // aresample=...:first_pts=0 re-bases the pulse stream's absolute
        // monotonic timestamps to 0 — without it the audio track starts at
        // ~6 s (pulse clock) while the count-based video starts at 0 and the
        // ogg muxer mis-interleaves the two (verified live: video 0.9 s /
        // audio start 6.07 s before the fix, both at 0.00 s / 4.9 s after).
        for s in [
            "-f", "pulse", "-i", "default", "-map", "0:v", "-map", "1:a", "-af",
            "aresample=48000:async=1:first_pts=0", "-c:a", "libvorbis", "-q:a", VORBIS_Q,
        ] {
            a.push(s.to_string());
        }
    } else {
        a.extend(["-map".into(), "0:v".into(), "-an".into()]);
    }
    a.extend([
        "-c:v".to_string(),
        "libtheora".to_string(),
        "-q:v".to_string(),
        THEORA_Q.to_string(),
        "-f".to_string(),
        "ogg".to_string(),
        path.to_string_lossy().into_owned(),
    ]);
    a
}

/// Record dimensions: never taller than RECORD_MAX_HEIGHT, always even
/// (Theora wants even luma dimensions).
fn rec_dims(w: u32, h: u32) -> (u32, u32) {
    if h <= RECORD_MAX_HEIGHT {
        return (w.max(2) & !1, h.max(2) & !1);
    }
    let scale = RECORD_MAX_HEIGHT as f64 / h as f64;
    let rw = ((w as f64 * scale) as u32).max(2) & !1;
    (rw, RECORD_MAX_HEIGHT & !1)
}

/// Local-time slug for recording filenames: YYYY-MM-DD_HH-MM-SS.
fn timestamp_slug() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    // SAFETY: localtime_r writes a fully-initialized tm into our zeroed
    // buffer; no shared state.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Default recording directory: $XDG_VIDEOS_DIR, else ~/Videos.
fn default_record_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_VIDEOS_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Videos"))
        .unwrap_or_else(|| PathBuf::from("Videos"))
}

/// A fresh, collision-free `easyfilters-<timestamp>.ogv` inside `dir`.
fn unique_in_dir(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))?;
    let slug = timestamp_slug();
    let mut p = dir.join(format!("easyfilters-{slug}.ogv"));
    let mut n = 2u32;
    while p.exists() {
        p = dir.join(format!("easyfilters-{slug}-{n}.ogv"));
        n += 1;
    }
    Ok(p)
}

/// Where the next recording goes. `explicit` (the `--record` CLI path) wins:
/// a directory (or trailing-slash path) gets a timestamped file inside it,
/// anything else is used verbatim as the file. Otherwise `dir` (config
/// `record_dir` / `--record-dir`), else the default videos dir.
pub(crate) fn resolve_output_path(dir: Option<&Path>, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_dir() || p.to_string_lossy().ends_with('/') {
            return unique_in_dir(p);
        }
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {}", parent.display()))?;
            }
        }
        return Ok(p.to_path_buf());
    }
    let base = dir.map(Path::to_path_buf).unwrap_or_else(default_record_dir);
    unique_in_dir(&base)
}

impl Recorder {
    /// Start recording the w×h feed at the negotiated interval, with the
    /// PulseAudio default input as the audio track.
    pub fn start(w: u32, h: u32, num: u32, den: u32, path: PathBuf) -> Result<Self> {
        Self::start_inner(w, h, num, den, path, true)
    }

    fn start_inner(w: u32, h: u32, num: u32, den: u32, path: PathBuf, audio: bool) -> Result<Self> {
        let (rw, rh) = rec_dims(w, h);
        let args = ffmpeg_args(rw, rh, den, num, audio, &path);
        // Make sure the output's parent dir exists (resolve_output_path does
        // this for generated paths, but an explicit `--record /a/b/c.ogv`
        // may point into a not-yet-existing directory).
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {}", parent.display()))?;
            }
        }
        // ffmpeg stderr goes to <path>.ffmpeg.log so failures are diagnosable.
        let mut log_os = path.clone().into_os_string();
        log_os.push(".ffmpeg.log");
        let log_path = PathBuf::from(log_os);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .with_context(|| format!("open {}", log_path.display()))?;
        let mut child = Command::new(ffmpeg_binary())
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| {
                format!(
                    "spawn {} (needs libtheora/libvorbis encoders and the pulse device — \
                     on NixOS use the wrapped binary, which ships ffmpeg-full; or set $EASYFILTERS_FFMPEG)",
                    ffmpeg_binary()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ffmpeg stdin not piped"))?;

        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(true));
        let frames = Arc::new(AtomicU64::new(0));
        let drops = Arc::new(AtomicU64::new(0));
        // v4l2 interval num/den = seconds per frame.
        let interval = Duration::from_secs_f64(num as f64 / den as f64);
        let writer = thread::Builder::new()
            .name("easyfilters-rec".into())
            .spawn({
                let slot = Arc::clone(&slot);
                let running = Arc::clone(&running);
                let frames = Arc::clone(&frames);
                move || writer_loop(stdin, slot, running, frames, interval, w, h, rw, rh)
            })
            .map_err(|e| anyhow!("spawn recorder writer thread: {e}"))?;

        info!(path = %path.display(), w, h, record = format!("{rw}x{rh}"), audio, "recorder started");
        Ok(Self {
            child,
            slot,
            running,
            writer: Some(writer),
            frames,
            drops,
            path,
        })
    }

    /// Hot path: hand one BGR24 frame to the writer. Latest-wins — if the
    /// writer has not consumed the previous frame yet, that frame is dropped
    /// (counted in `drops()`); the capture loop never blocks.
    pub fn write_frame(&self, bgr: &[u8]) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        let mut slot = self.slot.lock().unwrap();
        if slot.replace(Arc::from(bgr)).is_some() {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frames actually written to the ffmpeg pipe.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Frames overwritten in the slot before the writer consumed them.
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Stop recording: SIGINT lets ffmpeg finalize the ogg cleanly; the
    /// writer thread exits on the resulting EPIPE. Even a SIGKILL leaves a
    /// playable file (Ogg is streamable), just without the final pages.
    pub fn stop(self) {
        // Drop runs shutdown().
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // SAFETY: kill(2) with the child's pid; ESRCH when it already exited
        // is harmless.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        if let Some(w) = self.writer.take() {
            if w.join().is_err() {
                warn!("recorder writer thread panicked");
            }
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    info!(
                        path = %self.path.display(),
                        frames = self.frames.load(Ordering::Relaxed),
                        drops = self.drops.load(Ordering::Relaxed),
                        %status,
                        "recording stopped"
                    );
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    warn!("ffmpeg did not exit after SIGINT; SIGKILL (file stays playable)");
                    unsafe {
                        libc::kill(self.child.id() as i32, libc::SIGKILL);
                    }
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// Writer thread: write exactly one frame per `interval` (CFR pacing keeps
/// count-based PTS == wall clock), repeating the latest frame when the
/// producer dropped one. Downscales to rw×rh when the feed is taller than
/// RECORD_MAX_HEIGHT.
#[allow(clippy::too_many_arguments)]
fn writer_loop(
    mut stdin: ChildStdin,
    slot: FrameSlot,
    running: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    interval: Duration,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
) {
    let start = Instant::now();
    let mut tick = 0u64;
    let mut last: Option<Arc<[u8]>> = None;
    while running.load(Ordering::Relaxed) {
        let target = start + interval.mul_f64(tick as f64);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
        tick += 1;
        let fresh = slot.lock().unwrap().take();
        let current = match fresh {
            Some(f) => {
                last = Some(Arc::clone(&f));
                f
            }
            None => match &last {
                Some(f) => Arc::clone(f),
                None => continue, // nothing produced yet; don't advance PTS
            },
        };
        let written: &[u8] = if (rw, rh) == (w, h) {
            &current
        } else {
            // Downscale on this thread, off the capture hot path.
            let down = (|| -> Result<Vec<u8>> {
                let mut mat = Mat::new_rows_cols_with_default(
                    h as i32,
                    w as i32,
                    CV_8UC3,
                    Scalar::all(0.0),
                )?;
                mat.data_bytes_mut()?.copy_from_slice(&current);
                let small = frame::resize(&mat, rw, rh)?;
                Ok(small.data_bytes()?.to_vec())
            })();
            match down {
                Ok(buf) => {
                    // buf must outlive the write_all below — keep it in scope.
                    if stdin.write_all(&buf).is_err() {
                        break;
                    }
                    frames.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, "recorder downscale failed; skipping frame");
                    continue;
                }
            }
        };
        if stdin.write_all(written).is_err() {
            // ffmpeg exited (SIGINT on stop, or it died) — EPIPE.
            break;
        }
        frames.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("easyfilters-test-rec-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn ffmpeg_args_are_the_ogv_theora_vorbis_command() {
        let args = ffmpeg_args(960, 540, 30, 1, true, Path::new("/tmp/out.ogv"));
        let joined = args.join(" ");
        assert!(joined.contains("-f rawvideo"), "{joined}");
        assert!(joined.contains("-pix_fmt bgr24"), "{joined}");
        assert!(joined.contains("-s 960x540"), "{joined}");
        assert!(joined.contains("-r 30/1"), "{joined}"); // den/num = fps
        assert!(joined.contains("-i pipe:0"), "{joined}");
        assert!(joined.contains("-f pulse -i default"), "{joined}");
        // audio timestamps re-based to 0 so the ogv muxer interleaves A/V
        assert!(joined.contains("aresample=48000:async=1:first_pts=0"), "{joined}");
        assert!(joined.contains("-c:v libtheora"), "{joined}");
        assert!(joined.contains("-c:a libvorbis"), "{joined}");
        assert!(joined.contains("-f ogg /tmp/out.ogv"), "{joined}");
        // video-only variant must not reference pulse
        let v = ffmpeg_args(64, 48, 30, 1, false, Path::new("/tmp/v.ogv")).join(" ");
        assert!(!v.contains("pulse"), "{v}");
        assert!(v.contains("-an"), "{v}");
    }

    #[test]
    fn rec_dims_caps_height_and_keeps_even() {
        assert_eq!(rec_dims(1280, 720), (960, 540));
        assert_eq!(rec_dims(1920, 1080), (960, 540));
        assert_eq!(rec_dims(640, 360), (640, 360)); // small feeds untouched
        assert_eq!(rec_dims(1281, 721).1, 540);
        assert_eq!(rec_dims(1281, 721).0 % 2, 0);
    }

    #[test]
    fn timestamp_slug_looks_like_a_local_datetime() {
        let s = timestamp_slug();
        let parts: Vec<&str> = s.split(['-', '_']).collect();
        assert_eq!(parts.len(), 6, "{s}");
        assert!(parts.iter().all(|p| p.len() == 2 || p.len() == 4), "{s}");
        assert!(parts[0].parse::<u32>().unwrap() >= 2026, "{s}");
    }

    #[test]
    fn resolve_output_path_explicit_file_is_used_verbatim() {
        let d = tmp_dir("explicit");
        let p = d.join("sub").join("my.ogv");
        let got = resolve_output_path(None, Some(&p)).unwrap();
        assert_eq!(got, p);
        assert!(p.parent().unwrap().is_dir()); // parent created
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn resolve_output_path_dir_gets_timestamped_collision_free_name() {
        let d = tmp_dir("dir");
        let p1 = resolve_output_path(Some(&d), None).unwrap();
        assert!(p1.starts_with(&d));
        assert!(p1.file_name().unwrap().to_string_lossy().starts_with("easyfilters-"));
        assert!(p1.to_string_lossy().ends_with(".ogv"));
        // second call in the same second must not collide
        std::fs::write(&p1, b"x").unwrap();
        let p2 = resolve_output_path(Some(&d), None).unwrap();
        assert_ne!(p1, p2);
        assert!(p2.to_string_lossy().contains("-2.ogv"), "{}", p2.display());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// End-to-end against a real ffmpeg (skipped when no ffmpeg on PATH):
    /// record 1 s of synthetic frames video-only, stop, and check the file
    /// is an Ogg stream.
    #[test]
    fn record_smoke_produces_an_ogg_file() {
        if Command::new(ffmpeg_binary())
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping record_smoke: no ffmpeg on PATH");
            return;
        }
        let d = tmp_dir("smoke");
        let path = d.join("smoke.ogv");
        let rec = Recorder::start_inner(64, 48, 1, 30, path.clone(), false).unwrap();
        let frame = vec![128u8; 64 * 48 * 3];
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(1) {
            rec.write_frame(&frame);
            thread::sleep(Duration::from_millis(10));
        }
        let frames = rec.frames();
        rec.stop();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 100, "tiny file: {}", meta.len());
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..4], b"OggS", "not an ogg file");
        assert!(frames >= 25, "expected ~30 paced frames, got {frames}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
