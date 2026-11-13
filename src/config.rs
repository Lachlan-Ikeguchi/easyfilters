//! TOML configuration (milestone 4): `$XDG_CONFIG_HOME/easyfilters/config.toml`
//! by default, `--config <path>` to override. Every field is optional; the
//! effective value is CLI arg > config file > built-in default (see
//! `resolve` in main.rs). The GUI and tray persist changes immediately via
//! `save_current`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::settings::{Mode, Shared};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: Option<bool>,
    pub mode: Option<Mode>,
    pub blur_strength: Option<f64>,
    pub mask_smoothing: Option<f32>,
    pub background: Option<PathBuf>,
    pub gaze_correction: Option<bool>,
    pub gaze_strength: Option<f32>,
    /// Milestone 6: close the GUI window to the tray instead of quitting.
    pub close_to_tray: Option<bool>,
    /// Milestone 6: start with the window hidden (tray only).
    pub start_minimized: Option<bool>,
    /// Milestone 7: publish a native PipeWire camera node (default: on).
    pub pipewire: Option<bool>,
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub format: Option<String>,
    pub buffers: Option<u32>,
    pub model: Option<PathBuf>,
    pub face_model: Option<PathBuf>,
}

/// Default config location: `$XDG_CONFIG_HOME/easyfilters/config.toml`,
/// falling back to `$HOME/.config/easyfilters/config.toml`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("easyfilters").join("config.toml")
}

/// Load `path`. A missing file yields `Config::default()`; a malformed file is
/// an error (the caller decides whether to warn and continue).
pub fn load(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomically save `cfg` to `path`: write `<path>.tmp`, then rename, so a
/// crash mid-write cannot leave a torn config.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
    }
    let text = toml::to_string_pretty(cfg).context("serialize config")?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Build a `Config` from the live shared state and save it — this is what the
/// GUI/tray call after every change so the next launch resumes where this one
/// left off.
pub fn save_current(path: &Path, shared: &Shared) -> Result<()> {
    let s = shared.snapshot_settings();
    let d = shared.device.read().unwrap().clone();
    let cfg = Config {
        enabled: Some(s.enabled),
        mode: Some(s.mode),
        blur_strength: Some(s.blur_strength),
        mask_smoothing: Some(s.mask_smoothing),
        background: s.background.clone(),
        gaze_correction: Some(s.gaze_correction),
        gaze_strength: Some(s.gaze_strength),
        close_to_tray: Some(s.close_to_tray),
        start_minimized: Some(s.start_minimized),
        pipewire: Some(s.pipewire),
        input: d.input.clone(),
        output: d.output.clone(),
        width: Some(d.width),
        height: Some(d.height),
        fps: Some(d.fps),
        format: d.format.clone(),
        buffers: Some(d.buffers),
        model: d.model.clone(),
        face_model: d.face_model.clone(),
    };
    save(path, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("easyfilters-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn missing_file_loads_as_default() {
        let p = tmp_path("missing.toml");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load(&p).unwrap(), Config::default());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let p = tmp_path("malformed.toml");
        std::fs::write(&p, "this is [not toml").unwrap();
        assert!(load(&p).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn partial_file_fills_only_what_it_sets() {
        let p = tmp_path("partial.toml");
        std::fs::write(&p, "mode = \"blur\"\nblur_strength = 25.0\n").unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.mode, Some(Mode::Blur));
        assert_eq!(cfg.blur_strength, Some(25.0));
        assert_eq!(cfg.enabled, None);
        assert_eq!(cfg.width, None);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn save_then_load_roundtrips() {
        let p = tmp_path("roundtrip.toml");
        let _ = std::fs::remove_file(&p);
        let cfg = Config {
            enabled: Some(false),
            mode: Some(Mode::Replace),
            blur_strength: Some(22.5),
            mask_smoothing: Some(0.3),
            background: Some(PathBuf::from("/tmp/bg.jpg")),
            gaze_correction: Some(true),
            gaze_strength: Some(0.75),
            close_to_tray: Some(false),
            start_minimized: Some(true),
            pipewire: Some(true),
            input: Some(PathBuf::from("/dev/video1")),
            output: Some(PathBuf::from("/dev/video0")),
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            format: Some("MJPG".to_string()),
            buffers: Some(6),
            model: Some(PathBuf::from("/tmp/model.onnx")),
            face_model: Some(PathBuf::from("/tmp/face.onnx")),
        };
        save(&p, &cfg).unwrap();
        assert_eq!(load(&p).unwrap(), cfg);
        // no leftover tmp file after the atomic rename
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn save_creates_parent_dirs() {
        let p = tmp_path("deep/dir/config.toml");
        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
        save(&p, &Config::default()).unwrap();
        assert!(p.exists());
        std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn default_path_is_under_config_home() {
        let p = default_path();
        assert!(p.ends_with("easyfilters/config.toml"));
    }
}
