//! Single-instance lock (milestone 6): a non-blocking `flock` on
//! `$XDG_RUNTIME_DIR/easyfilters.lock`. With autostart, a second manual
//! launch would otherwise fail on the camera with a cryptic EBUSY — instead
//! it prints "already running (pid N)" and exits cleanly.
//!
//! The lock lives in the runtime dir (per-session tmpfs), so a stale lock
//! can never survive a reboot. If `XDG_RUNTIME_DIR` is unset the lock is
//! skipped with a warning rather than risk a stale lock in a persistent dir.
//!
//! `--list` / `--self-test` never call this (they don't touch devices).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use v4l2r::nix::libc;

pub const LOCK_FILE_NAME: &str = "easyfilters.lock";

/// Outcome of trying to take the lock.
pub enum Acquire {
    /// We hold the lock. Keep the `Lock` alive for the process lifetime;
    /// dropping it (or process exit) releases the `flock`.
    Locked(Lock),
    /// Another instance holds it; carries its pid if we could read it.
    Busy(Option<u32>),
}

pub struct Lock {
    /// Held for the process lifetime: the `flock` is released when this
    /// file is closed (drop or exit).
    #[allow(dead_code)]
    file: File,
}

/// Default lock path: `$XDG_RUNTIME_DIR/easyfilters.lock`, or `None` when
/// `XDG_RUNTIME_DIR` is unset/empty (caller skips the lock with a warning).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|d| d.join(LOCK_FILE_NAME))
}

/// Acquire the single-instance lock at the default path. `None` = no runtime
/// dir, lock skipped.
pub fn acquire() -> Option<io::Result<Acquire>> {
    default_path().map(|p| acquire_at(&p))
}

/// Take a non-blocking exclusive `flock` on `path`, writing our pid into it.
pub fn acquire_at(path: &Path) -> io::Result<Acquire> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new().read(true).write(true).create(true).open(path)?;
    // SAFETY: plain flock(2) on an fd we own; LOCK_NB makes it non-blocking.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            // Someone else holds it; try to read their pid for the message.
            let mut buf = String::new();
            let _ = (&file).read_to_string(&mut buf);
            let pid = buf.trim().parse::<u32>().ok();
            return Ok(Acquire::Busy(pid));
        }
        return Err(err);
    }
    // We hold the lock: record our pid (truncate first).
    let mut f = file;
    f.seek(SeekFrom::Start(0))?;
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    f.flush()?;
    Ok(Acquire::Locked(Lock { file: f }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("easyfilters-lock-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn second_lock_on_same_file_is_busy() {
        let p = tmp_path("busy.lock");
        let _ = std::fs::remove_file(&p);
        match acquire_at(&p).unwrap() {
            Acquire::Locked(first) => {
                // the pid was written
                let mut buf = String::new();
                let mut f = OpenOptions::new().read(true).open(&p).unwrap();
                f.read_to_string(&mut buf).unwrap();
                assert_eq!(buf.trim().parse::<u32>().unwrap(), std::process::id());
                // second acquire on the same path must report Busy
                match acquire_at(&p).unwrap() {
                    Acquire::Busy(pid) => assert_eq!(pid, Some(std::process::id())),
                    Acquire::Locked(_) => panic!("second lock must not succeed"),
                }
                drop(first);
            }
            Acquire::Busy(_) => panic!("first lock must succeed"),
        }
        // after dropping the first lock, a new acquire succeeds
        match acquire_at(&p).unwrap() {
            Acquire::Locked(_) => {}
            Acquire::Busy(_) => panic!("lock must be free after drop"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn flock_is_released_on_drop_not_only_close() {
        // Two handles in the same process: flock is per open-file-description,
        // so the second open+flock must fail while the first is held.
        let p = tmp_path("drop.lock");
        let _ = std::fs::remove_file(&p);
        let first = match acquire_at(&p).unwrap() {
            Acquire::Locked(l) => l,
            _ => panic!(),
        };
        assert!(first.file.as_raw_fd() >= 0);
        assert!(matches!(acquire_at(&p).unwrap(), Acquire::Busy(_)));
        drop(first);
        assert!(matches!(acquire_at(&p).unwrap(), Acquire::Locked(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn default_path_uses_runtime_dir() {
        // XDG_RUNTIME_DIR is set in this environment; just check the shape.
        if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
            let p = default_path().unwrap();
            assert!(p.ends_with(LOCK_FILE_NAME));
        }
    }
}
