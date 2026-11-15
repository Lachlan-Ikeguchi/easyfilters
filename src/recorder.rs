//! Recording: an ffmpeg subprocess writes the filtered feed (raw BGR24 on
//! stdin) plus the PulseAudio default input (`-f pulse -i default`) into an
//! .ogv (Ogg/Theora/Vorbis). No new cargo dependencies; ffmpeg is a runtime
//! dependency (nix wrapper prefixes PATH with ffmpeg-full; override with
//! $EASYFILTERS_FFMPEG).
//!
//! Sync design (verified experimentally):
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
//!
//! Spool mode (`--record-buffer` / `record_buffer`, the GUI "Buffer
//! recording" switch): nothing is encoded to video live. A spool thread
//! JPEG-compresses every produced frame (`frame::encode_jpeg`) and appends
//! it to `<path>.frames.mjpeg` on disk — ~10-20x smaller than raw BGR24 —
//! while the PulseAudio default input is captured in parallel to
//! `<path>.audio.ogg`. Only a small bounded RAM queue sits in front of the
//! disk, so the capture loop never stalls. When recording stops, a
//! background thread runs one ffmpeg pass (`-f mjpeg -r den/num -i <spool>`)
//! into the final .ogv. Because the spool holds exactly the frames the
//! camera produced, count-based PTS spans the full wall-clock duration at
//! the proper frame rate — this is the fix for recordings playing back sped
//! up when live encoding at full resolution falls behind. Tradeoffs: disk
//! space for the spool and stop-time processing; the spool files are deleted
//! after a successful encode. main() blocks on FINALIZING before exiting, so
//! quitting right after stopping a buffered recording still yields the
//! final .ogv.

use std::collections::VecDeque;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};
use v4l2r::nix::libc;

/// Theora video quality 0-10, Vorbis audio quality 0-10.
const THEORA_Q: &str = "5";
const VORBIS_Q: &str = "4";
/// How long to wait for ffmpeg to finalize after SIGINT before SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Spool-mode RAM queue cap in front of the disk: raw BGR24 frames waiting to
/// be JPEG-encoded (~40 frames at 1080p). Beyond this the oldest are dropped
/// (counted in `drops()`).
const SPOOL_QUEUE_CAP_BYTES: u64 = 256 * 1024 * 1024;
/// JPEG quality for spool frames (1-100). High, because the spool is an
/// intermediate and the final codec (Theora) is lossy anyway.
const SPOOL_JPEG_QUALITY: i32 = 92;

/// Number of spool-finalize threads currently running. main() waits for this
/// to reach 0 before exiting, so a quit right after stopping a buffered
/// recording still produces the final .ogv.
pub(crate) static FINALIZING: AtomicUsize = AtomicUsize::new(0);

/// Block until all running spool finalizations are done.
pub(crate) fn wait_for_finalize() {
    while FINALIZING.load(Ordering::Acquire) > 0 {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Progress of a running spool finalize, shown in the GUI status line.
#[derive(Clone, Debug)]
pub(crate) struct FinalizeProgress {
    pub path: String,
    /// Encode progress in permille (0..=1000).
    pub permille: u32,
}

static FINALIZE_PROGRESS: Mutex<Option<FinalizeProgress>> = Mutex::new(None);

/// Current spool-finalize progress, `None` when no encode is running.
pub fn finalize_progress() -> Option<FinalizeProgress> {
    FINALIZE_PROGRESS.lock().unwrap().clone()
}

fn set_finalize_progress(path: &Path, permille: u32) {
    *FINALIZE_PROGRESS.lock().unwrap() = Some(FinalizeProgress {
        path: path.display().to_string(),
        permille: permille.min(1000),
    });
}

fn clear_finalize_progress() {
    *FINALIZE_PROGRESS.lock().unwrap() = None;
}

/// Parse an ffmpeg `-progress` line into permille of `total_secs`. Pure
/// function so tests can assert on it.
///
/// Only `out_time_us=` and the `out_time=` timecode are trusted: recent ffmpeg
/// prints the legacy `out_time_ms=` key with *microsecond* values (verified
/// against ffmpeg 9.0: `out_time_us=1366667` and `out_time_ms=1366667` for
/// 1.37 s), so reading it as milliseconds would report ~1000x and pin the
/// bar at 100% on the first progress block.
fn progress_permille(line: &str, total_secs: f64) -> Option<u32> {
    // ffmpeg's final event: the muxer is done, the file is complete.
    if line.trim() == "progress=end" {
        return Some(1000);
    }
    let micros = if let Some(v) = line.strip_prefix("out_time_us=") {
        v.trim().parse::<u64>().ok()?
    } else if let Some(v) = line.strip_prefix("out_time=") {
        parse_timecode_us(v.trim())?
    } else {
        return None;
    };
    if total_secs <= 0.0 {
        return Some(0);
    }
    Some((((micros as f64 / 1e6 / total_secs) * 1000.0) as u32).min(1000))
}

/// Parse ffmpeg's `out_time=` timecode `HH:MM:SS.ffffff` into microseconds.
fn parse_timecode_us(s: &str) -> Option<u64> {
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let mut parts = hms.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec: u64 = parts.next()?.parse().ok()?;
    // pad/truncate the fraction to exactly 6 digits (microseconds)
    let frac = &frac[..frac.len().min(6)];
    let frac: u64 = format!("{frac:0<6}").parse().ok()?;
    Some((h * 3600 + m * 60 + sec) * 1_000_000 + frac)
}

/// Latest-wins frame slot between the capture loop and the writer thread.
type FrameSlot = Arc<Mutex<Option<Arc<[u8]>>>>;

/// `path` with `suffix` appended: `<path>.frames.mjpeg` etc.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Open (create/truncate) an ffmpeg stderr log file.
fn open_log(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// SIGINT a child (clean ffmpeg finalize) and reap it, escalating to SIGKILL
/// after STOP_TIMEOUT. Returns the exit status when the child was reaped.
fn sigint_and_reap(child: &mut Child) -> Option<ExitStatus> {
    // SAFETY: kill(2) with the child's pid; ESRCH when it already exited is
    // harmless.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                warn!("ffmpeg did not exit after SIGINT; SIGKILL (file stays playable)");
                // SAFETY: same as above.
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGKILL);
                }
                return child.wait().ok();
            }
        }
    }
}

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

/// Record dimensions: the full feed resolution (same as the preview), only
/// rounded to even (Theora wants even luma dimensions).
fn rec_dims(w: u32, h: u32) -> (u32, u32) {
    (w.max(2) & !1, h.max(2) & !1)
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
        let log_path = sibling(&path, ".ffmpeg.log");
        let log = open_log(&log_path)?;
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
                move || writer_loop(stdin, slot, running, frames, interval)
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
        if let Some(w) = self.writer.take() {
            if w.join().is_err() {
                warn!("recorder writer thread panicked");
            }
        }
        let status = sigint_and_reap(&mut self.child);
        info!(
            path = %self.path.display(),
            frames = self.frames.load(Ordering::Relaxed),
            drops = self.drops.load(Ordering::Relaxed),
            ?status,
            "recording stopped"
        );
    }
}

/// Common interface of the recorder backends, so the pipeline can hold either
/// the live ffmpeg-pipe [`Recorder`] or the disk-spool [`SpoolRecorder`].
pub trait RecSink: Send {
    /// Hot path: hand one BGR24 frame to the recorder. Never blocks.
    fn write_frame(&self, bgr: &[u8]);
    /// Frames accepted so far.
    fn frames(&self) -> u64;
    /// Frames dropped (latest-wins overwrite / spool queue cap overflow).
    fn drops(&self) -> u64;
    fn path(&self) -> &Path;
    /// Stop recording. Both backends do the real work in Drop.
    fn stop(self: Box<Self>);
}

impl RecSink for Recorder {
    fn write_frame(&self, bgr: &[u8]) {
        Recorder::write_frame(self, bgr)
    }
    fn frames(&self) -> u64 {
        Recorder::frames(self)
    }
    fn drops(&self) -> u64 {
        Recorder::drops(self)
    }
    fn path(&self) -> &Path {
        Recorder::path(self)
    }
    fn stop(self: Box<Self>) {
        drop(self)
    }
}

/// Start a recorder: `buffer` selects the disk-spool backend (frames are
/// JPEG-spooled to disk during recording, the .ogv is encoded when recording
/// stops), otherwise the live ffmpeg-pipe backend.
pub fn start_any(
    w: u32,
    h: u32,
    num: u32,
    den: u32,
    path: PathBuf,
    buffer: bool,
) -> Result<Box<dyn RecSink>> {
    if buffer {
        Ok(Box::new(SpoolRecorder::start(w, h, num, den, path)?))
    } else {
        Ok(Box::new(Recorder::start(w, h, num, den, path)?))
    }
}

/// Writer thread: write exactly one frame per `interval` (CFR pacing keeps
/// count-based PTS == wall clock), repeating the latest frame when the
/// producer dropped one. Frames go out at the full feed resolution — the
/// recording matches the preview.
fn writer_loop(
    mut stdin: ChildStdin,
    slot: FrameSlot,
    running: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    interval: Duration,
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
        if stdin.write_all(&current).is_err() {
            // ffmpeg exited (SIGINT on stop, or it died) — EPIPE.
            break;
        }
        frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Shared queue between the hot capture loop and the spool thread.
struct SpoolQ {
    q: VecDeque<Arc<[u8]>>,
    bytes: u64,
    closing: bool,
}
type SharedSpoolQ = Arc<(Mutex<SpoolQ>, Condvar)>;

/// Push `f` onto the spool queue, dropping the oldest frames when the queue
/// exceeds `cap` bytes (counted in `drops`). Caller must hold the queue lock.
fn push_bounded(g: &mut SpoolQ, cap: u64, drops: &AtomicU64, f: Arc<[u8]>) {
    g.q.push_back(f);
    g.bytes += g.q.back().unwrap().len() as u64;
    while g.bytes > cap {
        match g.q.pop_front() {
            Some(old) => {
                g.bytes -= old.len() as u64;
                drops.fetch_add(1, Ordering::Relaxed);
            }
            None => break,
        }
    }
}

/// Disk-spool recorder ("buffer only, encode at stop"). See the module docs.
pub struct SpoolRecorder {
    q: SharedSpoolQ,
    frames: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
    audio: Option<Child>,
    audio_ok: bool,
    spool_path: PathBuf,
    audio_path: PathBuf,
    out_path: PathBuf,
    w: u32,
    h: u32,
    num: u32,
    den: u32,
    started: Instant,
    spool_thread: Option<JoinHandle<()>>,
}

impl SpoolRecorder {
    pub fn start(w: u32, h: u32, num: u32, den: u32, path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {}", parent.display()))?;
            }
        }
        let spool_path = sibling(&path, ".frames.mjpeg");
        let audio_path = sibling(&path, ".audio.ogg");
        let file = std::fs::File::create(&spool_path)
            .with_context(|| format!("create spool {}", spool_path.display()))?;
        // Audio spool: pulse default input -> vorbis ogg (cheap live encode,
        // much smaller than PCM). Best effort — video-only if this fails.
        let audio = Command::new(ffmpeg_binary())
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-f",
                "pulse",
                "-i",
                "default",
                "-c:a",
                "libvorbis",
                "-q:a",
                VORBIS_Q,
                "-f",
                "ogg",
            ])
            .arg(&audio_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(open_log(&sibling(&path, ".audio.ffmpeg.log"))?))
            .spawn();
        let (audio, audio_ok) = match audio {
            Ok(c) => (Some(c), true),
            Err(e) => {
                warn!(error = %e, "audio spool failed; buffered recording will be video-only");
                (None, false)
            }
        };
        let q: SharedSpoolQ = Arc::new((
            Mutex::new(SpoolQ {
                q: VecDeque::new(),
                bytes: 0,
                closing: false,
            }),
            Condvar::new(),
        ));
        let frames = Arc::new(AtomicU64::new(0));
        let written = Arc::new(AtomicU64::new(0));
        let drops = Arc::new(AtomicU64::new(0));
        let spool_thread = thread::Builder::new()
            .name("easyfilters-spool".into())
            .spawn({
                let q = Arc::clone(&q);
                let written_t = Arc::clone(&written);
                move || {
                    spool_loop(
                        BufWriter::with_capacity(4 * 1024 * 1024, file),
                        q,
                        written_t,
                        w,
                        h,
                    )
                }
            })
            .map_err(|e| anyhow!("spawn spool thread: {e}"))?;
        info!(path = %path.display(), spool = %spool_path.display(), w, h, audio = audio_ok, "spool recorder started");
        Ok(Self {
            q,
            frames,
            written,
            drops,
            audio,
            audio_ok,
            spool_path,
            audio_path,
            out_path: path,
            w,
            h,
            num,
            den,
            started: Instant::now(),
            spool_thread: Some(spool_thread),
        })
    }

    /// Hot path: queue one raw BGR24 frame for the spool thread. The queue
    /// is bounded (drop-oldest past SPOOL_QUEUE_CAP_BYTES), so this never
    /// blocks on disk.
    pub fn write_frame(&self, bgr: &[u8]) {
        let (lock, cv) = &*self.q;
        let mut g = lock.lock().unwrap();
        push_bounded(&mut g, SPOOL_QUEUE_CAP_BYTES, &self.drops, Arc::from(bgr));
        self.frames.fetch_add(1, Ordering::Relaxed);
        cv.notify_one();
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    pub fn path(&self) -> &Path {
        &self.out_path
    }
}

impl RecSink for SpoolRecorder {
    fn write_frame(&self, bgr: &[u8]) {
        SpoolRecorder::write_frame(self, bgr)
    }
    fn frames(&self) -> u64 {
        SpoolRecorder::frames(self)
    }
    fn drops(&self) -> u64 {
        SpoolRecorder::drops(self)
    }
    fn path(&self) -> &Path {
        SpoolRecorder::path(self)
    }
    fn stop(self: Box<Self>) {
        drop(self)
    }
}

/// Spool thread: pop raw frames, JPEG-encode each, append to the spool file.
/// Exits when the queue is closed and drained, then flushes + syncs.
fn spool_loop(
    mut out: BufWriter<std::fs::File>,
    q: SharedSpoolQ,
    written: Arc<AtomicU64>,
    w: u32,
    h: u32,
) {
    let (lock, cv) = &*q;
    loop {
        let f = {
            let mut g = lock.lock().unwrap();
            loop {
                if let Some(f) = g.q.pop_front() {
                    g.bytes -= f.len() as u64;
                    break Some(f);
                }
                if g.closing {
                    break None;
                }
                g = cv.wait(g).unwrap();
            }
        };
        let f = match f {
            Some(f) => f,
            None => break,
        };
        let encoded = (|| -> Result<Vec<u8>> {
            let mat = crate::frame::bgr_mat(&f, w, h)?;
            crate::frame::encode_jpeg(&mat, SPOOL_JPEG_QUALITY)
        })();
        match encoded {
            Ok(jpg) => {
                if out.write_all(&jpg).is_err() {
                    warn!("spool write failed; stopping spool thread");
                    break;
                }
                written.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                warn!(error = %e, "JPEG encode failed; stopping spool thread");
                break;
            }
        }
    }
    let _ = out.flush();
    let _ = out.get_ref().sync_all();
}

/// The finalize pass: decode the concatenated-JPEG spool at the negotiated
/// fps (count-based PTS == wall clock, since the spool holds exactly the
/// produced frames), mux the audio spool when present, encode to .ogv.
/// Pure function so tests can assert on it.
fn spool_encode_args(
    den: u32,
    num: u32,
    spool: &Path,
    audio: Option<&Path>,
    out: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-f".into(),
        "mjpeg".into(),
        "-r".into(),
        format!("{den}/{num}"),
        "-i".into(),
        spool.to_string_lossy().into_owned(),
    ];
    match audio {
        Some(au) => {
            a.extend([
                "-i".to_string(),
                au.to_string_lossy().into_owned(),
                "-map".to_string(),
                "0:v".to_string(),
                "-map".to_string(),
                "1:a".to_string(),
                "-af".to_string(),
                "aresample=48000:async=1:first_pts=0".to_string(),
                "-c:a".to_string(),
                "libvorbis".to_string(),
                "-q:a".to_string(),
                VORBIS_Q.to_string(),
            ]);
        }
        None => {
            a.extend(["-map".to_string(), "0:v".to_string(), "-an".to_string()]);
        }
    }
    a.extend([
        "-c:v".to_string(),
        "libtheora".to_string(),
        "-q:v".to_string(),
        THEORA_Q.to_string(),
        "-f".to_string(),
        "ogg".to_string(),
        out.to_string_lossy().into_owned(),
    ]);
    a
}

impl Drop for SpoolRecorder {
    fn drop(&mut self) {
        // Close the queue and wait for the spool thread to drain + sync.
        {
            let (lock, cv) = &*self.q;
            let mut g = lock.lock().unwrap();
            g.closing = true;
            cv.notify_one();
        }
        if let Some(t) = self.spool_thread.take() {
            if t.join().is_err() {
                warn!("spool thread panicked");
            }
        }
        // Finalize the audio spool (SIGINT lets ffmpeg close the ogg cleanly).
        if let Some(mut child) = self.audio.take() {
            sigint_and_reap(&mut child);
        }
        // Mux the audio only if the spool actually produced bytes (the pulse
        // device may not exist).
        let audio = if self.audio_ok {
            match std::fs::metadata(&self.audio_path) {
                Ok(m) if m.len() > 0 => Some(self.audio_path.clone()),
                _ => None,
            }
        } else {
            None
        };
        // Encode the final .ogv on a background thread — stop() runs on the
        // capture loop, which must not block on a long encode. main() waits
        // for FINALIZING to drain before exiting.
        let args = spool_encode_args(self.den, self.num, &self.spool_path, audio.as_deref(), &self.out_path);
        let spool = self.spool_path.clone();
        let audio_path = self.audio_path.clone();
        let has_audio = audio.is_some();
        let out = self.out_path.clone();
        let log_path = sibling(&out, ".ffmpeg.log");
        let accepted = self.frames.load(Ordering::Relaxed);
        let spooled = self.written.load(Ordering::Relaxed);
        let dropped = self.drops.load(Ordering::Relaxed);
        // Expected output duration: the video track spans spooled/fps, but the
        // audio spool runs on wall clock and can be slightly longer (dropped
        // frames, capture start/stop skew) — ffmpeg's out_time covers both, so
        // take the max or the bar would hit 100% while audio is still muxed.
        let video_secs = spooled as f64 * self.num as f64 / self.den as f64;
        let total_secs = if has_audio {
            video_secs.max(self.started.elapsed().as_secs_f64())
        } else {
            video_secs
        };
        FINALIZING.fetch_add(1, Ordering::AcqRel);
        let spawned = thread::Builder::new()
            .name("easyfilters-rec-finalize".into())
            .spawn(move || {
                set_finalize_progress(&out, 0);
                let result = (|| -> Result<()> {
                    let log = open_log(&log_path)?;
                    // -progress pipe:1 makes ffmpeg print out_time_* lines on
                    // stdout; the GUI shows them as the finalize percentage.
                    let mut child = Command::new(ffmpeg_binary())
                        .args(&args)
                        .arg("-progress")
                        .arg("pipe:1")
                        .arg("-nostats")
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::from(log))
                        .spawn()
                        .with_context(|| format!("spawn {}", ffmpeg_binary()))?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or_else(|| anyhow!("ffmpeg stdout not piped"))?;
                    let mut reader = std::io::BufReader::new(stdout);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line)? == 0 {
                            break; // EOF: ffmpeg closed its stdout
                        }
                        if let Some(p) = progress_permille(&line, total_secs) {
                            set_finalize_progress(&out, p);
                        }
                    }
                    let status = child.wait()?;
                    if !status.success() {
                        anyhow::bail!("ffmpeg exited {status}; spool kept at {}", spool.display());
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&spool);
                        if has_audio {
                            let _ = std::fs::remove_file(&audio_path);
                        }
                        info!(path = %out.display(), accepted, spooled, dropped, "buffered recording finalized");
                    }
                    Err(e) => {
                        warn!(error = %e, spool = %spool.display(), "finalize failed; spool files kept");
                    }
                }
                clear_finalize_progress();
                FINALIZING.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            FINALIZING.fetch_sub(1, Ordering::AcqRel);
            warn!("could not spawn finalize thread; spool kept at {}", self.spool_path.display());
        }
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
    fn rec_dims_keeps_full_resolution_and_rounds_even() {
        // The recording matches the preview: no downscale cap.
        assert_eq!(rec_dims(1280, 720), (1280, 720));
        assert_eq!(rec_dims(1920, 1080), (1920, 1080));
        assert_eq!(rec_dims(640, 360), (640, 360));
        assert_eq!(rec_dims(1281, 721), (1280, 720));
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

    #[test]
    fn spool_encode_args_decode_mjpeg_spool_and_mux_audio() {
        let a = spool_encode_args(
            30,
            1,
            Path::new("/tmp/s.frames.mjpeg"),
            Some(Path::new("/tmp/s.audio.ogg")),
            Path::new("/tmp/out.ogv"),
        );
        let joined = a.join(" ");
        assert!(joined.contains("-f mjpeg"), "{joined}");
        assert!(joined.contains("-r 30/1"), "{joined}"); // den/num = fps
        assert!(joined.contains("-i /tmp/s.frames.mjpeg"), "{joined}");
        assert!(joined.contains("-i /tmp/s.audio.ogg"), "{joined}");
        // audio timestamps re-based to 0 so the ogv muxer interleaves A/V
        assert!(joined.contains("aresample=48000:async=1:first_pts=0"), "{joined}");
        assert!(joined.contains("-c:v libtheora"), "{joined}");
        assert!(joined.contains("-c:a libvorbis"), "{joined}");
        assert!(joined.contains("-f ogg /tmp/out.ogv"), "{joined}");
        // video-only variant must not reference the audio spool
        let v = spool_encode_args(30, 1, Path::new("/tmp/s"), None, Path::new("/tmp/v.ogv")).join(" ");
        assert!(v.contains("-an"), "{v}");
        assert!(!v.contains("audio"), "{v}");
    }

    #[test]
    fn push_bounded_drops_oldest_beyond_cap() {
        let mut g = SpoolQ {
            q: VecDeque::new(),
            bytes: 0,
            closing: false,
        };
        let drops = AtomicU64::new(0);
        for i in 0..5u8 {
            push_bounded(&mut g, 30, &drops, Arc::from(vec![i; 10].into_boxed_slice()));
        }
        assert_eq!(g.q.len(), 3, "cap 30 bytes / 10-byte frames keeps 3");
        assert_eq!(g.bytes, 30);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(&g.q[0][..], &[2u8; 10][..], "oldest dropped first");
    }

    #[test]
    fn progress_permille_parses_out_time_lines() {
        assert_eq!(progress_permille("out_time_us=5000000\n", 10.0), Some(500));
        assert_eq!(progress_permille("out_time=00:00:05.000000\n", 10.0), Some(500));
        assert_eq!(progress_permille("out_time=00:01:40\n", 100.0), Some(1000));
        // the legacy out_time_ms key carries microseconds in ffmpeg >= 8 —
        // it must be ignored, not multiplied by 1000
        assert_eq!(progress_permille("out_time_ms=1366667\n", 10.0), None);
        assert_eq!(progress_permille("out_time_us=20000000\n", 10.0), Some(1000)); // capped
        assert_eq!(progress_permille("progress=end\n", 10.0), Some(1000)); // muxer done
        assert_eq!(progress_permille("frame=42\n", 10.0), None);
        assert_eq!(progress_permille("out_time_us=1000000\n", 0.0), Some(0));
    }

    /// End-to-end against a real ffmpeg (skipped when no ffmpeg on PATH):
    /// spool 40 synthetic frames, stop, and check the finalize thread
    /// produces an Ogg file and removes the spool.
    #[test]
    fn spool_smoke_encodes_ogv_on_stop() {
        if Command::new(ffmpeg_binary())
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping spool_smoke: no ffmpeg on PATH");
            return;
        }
        let d = tmp_dir("spool");
        let path = d.join("spool.ogv");
        let spool = sibling(&path, ".frames.mjpeg");
        let rec = SpoolRecorder::start(64, 48, 1, 30, path.clone()).unwrap();
        let frame = vec![180u8; 64 * 48 * 3];
        for _ in 0..40 {
            rec.write_frame(&frame);
        }
        assert_eq!(rec.frames(), 40);
        drop(rec); // drain spool, spawn finalize thread
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if path.exists() && std::fs::metadata(&path).unwrap().len() > 100 {
                break;
            }
            assert!(Instant::now() < deadline, "finalize did not produce {}", path.display());
            thread::sleep(Duration::from_millis(100));
        }
        wait_for_finalize();
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..4], b"OggS", "not an ogg file");
        assert!(!spool.exists(), "spool should be removed after a successful encode");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
