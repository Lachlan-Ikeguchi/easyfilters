//! System tray icon (milestone 4): `tray-item` with the ksni backend — a
//! pure-Rust StatusNotifierItem over D-Bus, so no gtk3/libappindicator system
//! libraries are needed and it works on any SNI host (KDE Plasma, COSMIC, …).
//! Menu: toggle filtering on/off (persisted to the config), quit. Failure to
//! register (no StatusNotifierWatcher on the session bus) is a warning, never
//! fatal.

use std::path::{Path, PathBuf};
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
}
