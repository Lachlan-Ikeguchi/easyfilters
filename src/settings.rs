//! Shared runtime state between the pipeline, GUI and tray threads
//! (milestone 4). Everything here is designed to be cheap to read from the
//! hot capture loop: the per-frame path only takes an uncontended `RwLock`
//! read plus a few atomic loads.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// What gets written to the virtual camera. One enum serves the CLI
/// (`ValueEnum`), the config file (serde, lowercase) and the GUI picker.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Milestone 1 behavior: copy the input MJPG frame verbatim.
    Passthrough,
    /// Frame with the segmented person kept, background dimmed/tinted.
    Overlay,
    /// Pure grayscale person mask.
    Mask,
    /// Person kept sharp, background Gaussian-blurred (blur_strength).
    Blur,
    /// Person kept sharp, background replaced (background image or #003200).
    Replace,
}

impl Mode {
    /// All modes, for the GUI picker.
    pub const ALL: [Mode; 5] = [
        Mode::Passthrough,
        Mode::Overlay,
        Mode::Mask,
        Mode::Blur,
        Mode::Replace,
    ];
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Passthrough => "passthrough",
            Mode::Overlay => "overlay",
            Mode::Mask => "mask",
            Mode::Blur => "blur",
            Mode::Replace => "replace",
        })
    }
}

/// Runtime-mutable filter settings. Written by the GUI/tray, snapshotted by
/// the pipeline every frame.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Master on/off. When false the pipeline writes passthrough regardless
    /// of `mode` (this is the tray toggle).
    pub enabled: bool,
    pub mode: Mode,
    /// Gaussian blur sigma for blur mode, expressed at 720p (scaled with the
    /// actual frame height).
    pub blur_strength: f64,
    /// EMA alpha for mask temporal smoothing, 0..1 (higher = smoother, laggier).
    pub mask_smoothing: f32,
    /// Background image for replace mode (None = solid #003200).
    pub background: Option<PathBuf>,
    /// Milestone 5: gaze correction (capped pupil shift). Opt-in, default
    /// off — the one effect that can look uncanny.
    pub gaze_correction: bool,
    /// Gaze shift scale, 0..=1 (the absolute pixel cap applies on top).
    pub gaze_strength: f32,
    /// Milestone 6: closing the GUI window hides it to the tray instead of
    /// quitting (filtering keeps running; tray "Quit" still stops the app).
    pub close_to_tray: bool,
    /// Milestone 6: start with the window hidden (tray only). `--background`
    /// sets this for one run; the config can persist it.
    pub start_minimized: bool,
    /// Milestone 7: publish the filtered feed as a native PipeWire camera
    /// node (`media.role = Camera`), next to the v4l2loopback device. On by
    /// default — it is the only path PipeWire-based apps (Snapshot, cosmic
    /// camera, browsers via the portal) can consume on the pipewire builds
    /// that crash on v4l2loopback streams.
    pub pipewire: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: Mode::Replace,
            blur_strength: 15.0,
            mask_smoothing: 0.6,
            background: None,
            gaze_correction: false,
            gaze_strength: 0.5,
            close_to_tray: true,
            start_minimized: false,
            pipewire: true,
        }
    }
}

/// Device/stream parameters. Changing any of these restarts the stream: the
/// pipeline watches `Shared::device_generation` and re-opens/re-negotiates.
#[derive(Clone, Debug)]
pub struct DeviceParams {
    /// None = auto-detect first uvcvideo capture node.
    pub input: Option<PathBuf>,
    /// None = auto-discover by card label.
    pub output: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Pixel format fourcc, e.g. "MJPG" (None = auto, prefer MJPG).
    pub format: Option<String>,
    /// Number of MMAP buffers per queue.
    pub buffers: u32,
    /// ONNX model override (None = $EASYFILTERS_MODEL or the bundled model).
    pub model: Option<PathBuf>,
    /// Face-mesh ONNX model override for gaze correction (None =
    /// $EASYFILTERS_FACE_MODEL or the bundled model). The face detector
    /// stays bundled ($EASYFILTERS_FACE_DETECTOR overrides it for packaging).
    pub face_model: Option<PathBuf>,
}

impl Default for DeviceParams {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            width: 1280,
            height: 720,
            fps: 30,
            format: None,
            buffers: 4,
            model: None,
            face_model: None,
        }
    }
}

/// One preview frame published by the pipeline: tightly packed RGBA8,
/// row-major, plus a sequence number so consumers detect new frames cheaply.
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

/// Pipeline statistics, refreshed every 2 s; the GUI shows them in a status
/// line and the pipeline logs them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    pub drops: u64,
    pub fps: f64,
    pub decode_ms: f64,
    pub infer_ms: f64,
    pub composite_ms: f64,
    pub encode_ms: f64,
    /// Milestone 5 gaze stage.
    pub gaze_ms: f64,
    /// Milestone 6: the format actually negotiated with the input camera
    /// (width, height, fps) — shown in the GUI status line. Set once per
    /// stream (after `negotiate`), carried by the 2 s stats write.
    pub negotiated: Option<(u32, u32, f64)>,
}

/// The single source of truth shared by all threads. `running` is backed by
/// the process-global `RUNNING` atomic in main.rs so the signal handler
/// (which cannot capture) can stop everything.
pub struct Shared {
    pub settings: RwLock<Settings>,
    pub device: RwLock<DeviceParams>,
    /// Bumped when `settings.background` changes; the pipeline reloads the
    /// background image only when this moved.
    pub bg_generation: AtomicU64,
    /// Bumped when `device` changes; the pipeline restarts the stream.
    pub device_generation: AtomicU64,
    /// Latest output frame as RGBA (GUI preview). Only written when
    /// `preview_on` is set (GUI attached).
    pub preview: Mutex<Option<PreviewFrame>>,
    pub preview_on: AtomicBool,
    pub stats: Mutex<Stats>,
    /// Milestone 6: tray -> GUI "show/hide the window" requests. The GUI
    /// watches edges of this on its tick and closes/reopens the window.
    /// Initialized from `!settings.start_minimized`.
    pub show_window: AtomicBool,
}

impl Shared {
    pub fn new(settings: Settings, device: DeviceParams) -> Self {
        let show_window = !settings.start_minimized;
        Self {
            settings: RwLock::new(settings),
            device: RwLock::new(device),
            bg_generation: AtomicU64::new(0),
            device_generation: AtomicU64::new(0),
            preview: Mutex::new(None),
            preview_on: AtomicBool::new(false),
            stats: Mutex::new(Stats::default()),
            show_window: AtomicBool::new(show_window),
        }
    }

    /// Process-global shutdown flag (set by the signal handler).
    pub fn running(&self) -> bool {
        crate::RUNNING.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        crate::RUNNING.store(false, Ordering::SeqCst);
    }

    pub fn snapshot_settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Publish a preview frame. No-op when no GUI is attached.
    pub fn publish_preview(&self, width: u32, height: u32, rgba: Vec<u8>, seq: u64) {
        if !self.preview_on.load(Ordering::Relaxed) {
            return;
        }
        *self.preview.lock().unwrap() = Some(PreviewFrame {
            width,
            height,
            rgba,
            seq,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_display_matches_clap_names() {
        // clap's ValueEnum derive kebab-cases variant names; all our variants
        // are single words, so Display must match them exactly for the GUI
        // picker and CLI to agree.
        for m in Mode::ALL {
            let v = m.to_possible_value().unwrap();
            assert_eq!(m.to_string(), v.get_name());
        }
    }

    #[test]
    fn mode_serde_roundtrip_is_lowercase() {
        for m in Mode::ALL {
            let v = toml::Value::try_from(m).unwrap();
            assert_eq!(v.as_str().unwrap(), m.to_string());
            assert_eq!(Mode::deserialize(v).unwrap(), m);
        }
    }

    #[test]
    fn shared_show_window_reflects_start_minimized() {
        let mut s = Settings::default();
        assert!(!s.start_minimized);
        assert!(Shared::new(s.clone(), DeviceParams::default()).show_window.load(Ordering::Relaxed));
        s.start_minimized = true;
        assert!(!Shared::new(s, DeviceParams::default()).show_window.load(Ordering::Relaxed));
        // close_to_tray defaults to true (run-in-background behavior)
        assert!(Settings::default().close_to_tray);
    }

    #[test]
    fn shared_publish_preview_respects_preview_on() {
        let shared = Shared::new(Settings::default(), DeviceParams::default());
        shared.publish_preview(2, 2, vec![0; 16], 1);
        assert!(shared.preview.lock().unwrap().is_none());
        shared.preview_on.store(true, Ordering::Relaxed);
        shared.publish_preview(2, 2, vec![7; 16], 2);
        let p = shared.preview.lock().unwrap().take().unwrap();
        assert_eq!(p.seq, 2);
        assert_eq!(p.rgba, vec![7; 16]);
    }
}
