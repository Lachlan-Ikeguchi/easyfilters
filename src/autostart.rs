//! XDG autostart entry (milestone 6, "launch at login"): `--install-autostart`
//! writes `~/.config/autostart/easyfilters.desktop` so the desktop session
//! starts easyfilters at login with the window hidden (`--start-minimized`);
//! `--uninstall-autostart` removes it. The NixOS module has an equivalent
//! systemd-user-service option for declarative setups.
//!
//! Paths are passed in explicitly (not read from env inside) so unit tests
//! can point them at a tmp dir.

use std::io;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "easyfilters.desktop";

/// Default autostart dir: `$XDG_CONFIG_HOME/autostart`, falling back to
/// `$HOME/.config/autostart`.
pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("autostart")
}

/// makeWrapper installs the real ELF as `.<name>-wrapped` and a shell script
/// as `<name>`; on Linux `current_exe()` (/proc/self/exe) resolves to the
/// *inner* binary after the wrapper `exec`s it. Rewrite the path to the
/// wrapper script so the autostart entry keeps the wrapper's env
/// (EASYFILTERS_* model paths, LD_LIBRARY_PATH). Pure rename; the caller
/// falls back to the original path if the wrapper doesn't exist.
pub fn wrapper_path(exe: &Path) -> PathBuf {
    match exe.file_name().and_then(|n| n.to_str()) {
        Some(name) => match name
            .strip_prefix('.')
            .and_then(|s| s.strip_suffix("-wrapped"))
        {
            Some(stem) if stem == "easyfilters" => exe.with_file_name(stem),
            _ => exe.to_path_buf(),
        },
        None => exe.to_path_buf(),
    }
}

/// The `.desktop` file content. `Exec` uses the absolute path of the running
/// binary with `--start-minimized` (tray only — filtering still runs).
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=EasyFilters\n\
         Comment=Virtual camera with background filters\n\
         Exec={} --start-minimized\n\
         Icon=easyfilters\n\
         Terminal=false\n\
         Categories=AudioVideo;Video;\n\
         X-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

/// Write the autostart entry into `dir` (created if needed), atomically
/// (tmp + rename, like `config::save`). Idempotent. Returns the file path.
pub fn install_at(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, desktop_entry(exe))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Remove the autostart entry from `dir`. Returns whether a file was removed.
pub fn uninstall_at(dir: &Path) -> io::Result<bool> {
    match std::fs::remove_file(dir.join(FILE_NAME)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("easyfilters-autostart-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn desktop_entry_points_at_exe_with_start_minimized() {
        let text = desktop_entry(Path::new("/nix/store/abc-easyfilters/bin/easyfilters"));
        assert!(text.contains("Type=Application"));
        assert!(text.contains(
            "Exec=/nix/store/abc-easyfilters/bin/easyfilters --start-minimized"
        ));
        assert!(text.contains("Icon=easyfilters"));
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn install_writes_file_and_is_idempotent() {
        let dir = tmp_dir("install");
        let _ = std::fs::remove_dir_all(&dir);
        let exe = Path::new("/tmp/easyfilters");
        let p = install_at(&dir, exe).unwrap();
        assert!(p.ends_with(FILE_NAME));
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("Exec=/tmp/easyfilters --start-minimized"));
        // no leftover tmp file
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());
        // idempotent: second install overwrites cleanly
        install_at(&dir, exe).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), content);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uninstall_reports_whether_something_was_removed() {
        let dir = tmp_dir("uninstall");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!uninstall_at(&dir).unwrap()); // nothing there
        install_at(&dir, Path::new("/tmp/easyfilters")).unwrap();
        assert!(uninstall_at(&dir).unwrap());
        assert!(!uninstall_at(&dir).unwrap()); // already gone
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_dir_is_under_config_home() {
        let d = default_dir();
        assert!(d.ends_with("autostart"));
    }

    #[test]
    fn wrapper_path_rewrites_inner_binary_to_wrapper() {
        // nix makeWrapper layout: bin/easyfilters (script) + bin/.easyfilters-wrapped (ELF)
        assert_eq!(
            wrapper_path(Path::new("/nix/store/abc-easyfilters-0.2.0/bin/.easyfilters-wrapped")),
            PathBuf::from("/nix/store/abc-easyfilters-0.2.0/bin/easyfilters")
        );
        // a plain (non-wrapped) binary is left alone
        assert_eq!(
            wrapper_path(Path::new("/home/u/easyfilters/target/release/easyfilters")),
            PathBuf::from("/home/u/easyfilters/target/release/easyfilters")
        );
        assert_eq!(
            wrapper_path(Path::new("/usr/bin/easyfilters")),
            PathBuf::from("/usr/bin/easyfilters")
        );
    }
}
