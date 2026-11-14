//! System tray icon (milestone 4): `tray-item` with the ksni backend — a
//! pure-Rust StatusNotifierItem over D-Bus, so no gtk3/libappindicator system
//! libraries are needed and it works on any SNI host (KDE Plasma, COSMIC, …).
//! Menu: toggle filtering on/off (persisted to the config), toggle gaze
//! correction, toggle feed mirroring, show/hide the GUI window
//! (milestone 6 — this is what makes
//! "run in the background" usable: close the window and the tray keeps the
//! app alive), quit. Failure to register (no StatusNotifierWatcher on the
//! session bus) is a warning, never fatal.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tracing::{info, warn};
use tray_item::{IconSource, TrayItem};

use crate::settings::Shared;

/// 32x32 icon in ARGB32 network byte order (the SNI pixmap format): a green
/// square with a transparent 2 px border. Generated in code — no asset file.
fn icon() -> IconSource {
    let (w, h) = (32i32, 32i32);
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if (2..w - 2).contains(&x) && (2..h - 2).contains(&y) {
                // A, R, G, B
                data[i..i + 4].copy_from_slice(&[0xFF, 0x00, 0x8C, 0x32]);
            }
        }
    }
    IconSource::Data {
        width: w,
        height: h,
        data,
    }
}

/// Flip the enabled flag and persist the config. Called by the tray menu item
/// (and unit-tested directly).
pub(crate) fn toggle_filtering(shared: &Shared, config_path: &Path) {
    let enabled = {
        let mut s = shared.settings.write().unwrap();
        s.enabled = !s.enabled;
        s.enabled
    };
    if let Err(e) = crate::config::save_current(config_path, shared) {
        warn!(error = %e, "could not save config after toggle");
    }
    info!(enabled, "toggled filtering");
}

/// Flip the gaze-correction flag and persist the config (milestone 5).
pub(crate) fn toggle_gaze(shared: &Shared, config_path: &Path) {
    let on = {
        let mut s = shared.settings.write().unwrap();
        s.gaze_correction = !s.gaze_correction;
        s.gaze_correction
    };
    if let Err(e) = crate::config::save_current(config_path, shared) {
        warn!(error = %e, "could not save config after gaze toggle");
    }
    info!(gaze_correction = on, "toggled gaze correction");
}

/// Flip the mirror flag and persist the config.
pub(crate) fn toggle_mirror(shared: &Shared, config_path: &Path) {
    let on = {
        let mut s = shared.settings.write().unwrap();
        s.mirror = !s.mirror;
        s.mirror
    };
    if let Err(e) = crate::config::save_current(config_path, shared) {
        warn!(error = %e, "could not save config after mirror toggle");
    }
    info!(mirror = on, "toggled feed mirroring");
}

/// Spawn the tray thread. The returned handle finishes when `shared` stops.
pub fn spawn(shared: Arc<Shared>, config_path: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut tray = match TrayItem::new("EasyFilters", icon()) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    error = %e,
                    "could not create tray icon (no StatusNotifierWatcher on the session bus?); continuing without tray"
                );
                return;
            }
        };

        let toggle_shared = shared.clone();
        let toggle_config = config_path.clone();
        if let Err(e) = tray.add_menu_item("Toggle filtering", move || {
            toggle_filtering(&toggle_shared, &toggle_config);
        }) {
            warn!(error = %e, "tray: could not add toggle item");
        }

        let gaze_shared = shared.clone();
        let gaze_config = config_path.clone();
        if let Err(e) = tray.add_menu_item("Toggle gaze correction", move || {
            toggle_gaze(&gaze_shared, &gaze_config);
        }) {
            warn!(error = %e, "tray: could not add gaze toggle item");
        }

        let mirror_shared = shared.clone();
        let mirror_config = config_path.clone();
        if let Err(e) = tray.add_menu_item("Toggle mirror", move || {
            toggle_mirror(&mirror_shared, &mirror_config);
        }) {
            warn!(error = %e, "tray: could not add mirror toggle item");
        }

        // Milestone 6: show/hide the GUI window. The GUI watches edges of
        // `show_window` on its tick and closes/reopens the window
        // (`window::close`/`window::open` — a real hide, since winit cannot
        // unmap a Wayland window).
        let win_shared = shared.clone();
        if let Err(e) = tray.add_menu_item("Show/Hide window", move || {
            let visible = !win_shared.show_window.load(Ordering::Relaxed);
            win_shared.show_window.store(visible, Ordering::Relaxed);
            info!(visible, "tray: show/hide window");
        }) {
            warn!(error = %e, "tray: could not add show/hide item");
        }

        let quit_shared = shared.clone();
        if let Err(e) = tray.add_menu_item("Quit", move || {
            info!("tray: quit requested");
            quit_shared.stop();
        }) {
            warn!(error = %e, "tray: could not add quit item");
        }

        info!("tray icon registered");
        // Keep the TrayItem alive until shutdown; dropping it unregisters the
        // icon from the watcher.
        while shared.running() {
            thread::sleep(Duration::from_millis(200));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DeviceParams, Settings};

    #[test]
    fn toggle_filtering_flips_and_persists() {
        let dir = std::env::temp_dir().join(format!("easyfilters-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(Settings::default(), DeviceParams::default());
        assert!(shared.snapshot_settings().enabled);

        toggle_filtering(&shared, &path);
        assert!(!shared.snapshot_settings().enabled);
        let cfg = crate::config::load(&path).unwrap();
        assert_eq!(cfg.enabled, Some(false));
        assert_eq!(cfg.mode, Some(crate::settings::Mode::Replace));

        toggle_filtering(&shared, &path);
        assert!(shared.snapshot_settings().enabled);
        assert_eq!(crate::config::load(&path).unwrap().enabled, Some(true));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn show_hide_window_flips_the_atomic() {
        let shared = Shared::new(Settings::default(), DeviceParams::default());
        assert!(shared.show_window.load(Ordering::Relaxed)); // visible by default
        // same flip the tray menu item performs
        let v = !shared.show_window.load(Ordering::Relaxed);
        shared.show_window.store(v, Ordering::Relaxed);
        assert!(!shared.show_window.load(Ordering::Relaxed));
    }

    #[test]
    fn toggle_gaze_flips_and_persists() {
        let dir = std::env::temp_dir().join(format!("easyfilters-test-gaze-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(Settings::default(), DeviceParams::default());
        assert!(!shared.snapshot_settings().gaze_correction); // opt-in default

        toggle_gaze(&shared, &path);
        assert!(shared.snapshot_settings().gaze_correction);
        let cfg = crate::config::load(&path).unwrap();
        assert_eq!(cfg.gaze_correction, Some(true));
        assert_eq!(cfg.gaze_strength, Some(0.5)); // persisted alongside

        toggle_gaze(&shared, &path);
        assert!(!shared.snapshot_settings().gaze_correction);
        assert_eq!(crate::config::load(&path).unwrap().gaze_correction, Some(false));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_mirror_flips_and_persists() {
        let dir = std::env::temp_dir().join(format!("easyfilters-test-mirror-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(Settings::default(), DeviceParams::default());
        assert!(!shared.snapshot_settings().mirror); // off by default

        toggle_mirror(&shared, &path);
        assert!(shared.snapshot_settings().mirror);
        assert_eq!(crate::config::load(&path).unwrap().mirror, Some(true));

        toggle_mirror(&shared, &path);
        assert!(!shared.snapshot_settings().mirror);
        assert_eq!(crate::config::load(&path).unwrap().mirror, Some(false));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
