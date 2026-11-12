//! Profile directory layout.
//!
//! A profile is a caller-owned directory:
//!
//! ```text
//! <dir>/
//!   user-data/         browser profile (Chromium --user-data-dir or Firefox -profile)
//!   fingerprint.json   seed and launch-time identity, written once
//! ```
//!
//! Existing directories in this layout load unchanged: the browser opens the
//! same `user-data/` (cookies, storage, sessions) and the fingerprint file is
//! read as-is and never rewritten.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fingerprint::{Fingerprint, FingerprintDefaults};

/// A profile directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    name: String,
    dir: PathBuf,
}

impl Profile {
    /// The profile at `dir`; `name` seeds a fingerprint created for it.
    pub fn new(name: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            dir: dir.into(),
        }
    }

    /// Profile name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Profile directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Browser data directory.
    pub fn user_data_dir(&self) -> PathBuf {
        self.dir.join("user-data")
    }

    /// Fingerprint file.
    pub fn fingerprint_path(&self) -> PathBuf {
        self.dir.join("fingerprint.json")
    }

    /// Load the fingerprint, creating it from `defaults` on first use.
    pub fn fingerprint(&self, defaults: &FingerprintDefaults) -> Result<Fingerprint> {
        std::fs::create_dir_all(self.user_data_dir())?;
        Fingerprint::load_or_create(&self.fingerprint_path(), &self.name, defaults)
    }
}

/// Remove Chromium's `SingletonLock` family when the process that holds it is
/// gone. Refuses with [`Error::Profile`] when a live process on this host
/// still holds the profile, so two browsers never share one profile.
pub fn clear_stale_singleton(user_data_dir: &Path) -> Result<()> {
    let lock = user_data_dir.join("SingletonLock");
    let target = match std::fs::read_link(&lock) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            return Err(Error::Profile(format!(
                "{} is not a symlink; refusing to touch it",
                lock.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    // Target is "<hostname>-<pid>".
    let text = target.to_string_lossy();
    let (host, pid) = text
        .rsplit_once('-')
        .and_then(|(h, p)| p.parse::<u32>().ok().map(|p| (h.to_string(), p)))
        .ok_or_else(|| Error::Profile(format!("unreadable profile lock target {text:?}")))?;
    let this_host = hostname().ok_or_else(|| Error::Profile("cannot read hostname".into()))?;
    if host != this_host {
        return Err(Error::Profile(format!(
            "profile is locked by another host ({host}); refusing to share it"
        )));
    }
    if process_alive(pid) {
        return Err(Error::Profile(format!(
            "profile is in use by running process {pid}"
        )));
    }
    for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let p = user_data_dir.join(name);
        if p.symlink_metadata().is_ok() {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn process_alive(pid: u32) -> bool {
    // A zombie has exited; it holds nothing.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_existing_profile_shape() {
        let p = Profile::new("work", "/data/browser-profiles/work");
        assert_eq!(
            p.user_data_dir(),
            PathBuf::from("/data/browser-profiles/work/user-data")
        );
        assert_eq!(
            p.fingerprint_path(),
            PathBuf::from("/data/browser-profiles/work/fingerprint.json")
        );
    }

    #[test]
    fn stale_lock_is_cleared_and_live_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let host = hostname().unwrap();
        let lock = dir.path().join("SingletonLock");
        std::os::unix::fs::symlink(format!("{host}-999999999"), &lock).unwrap();
        std::os::unix::fs::symlink("cookie", dir.path().join("SingletonCookie")).unwrap();
        clear_stale_singleton(dir.path()).unwrap();
        assert!(lock.symlink_metadata().is_err());
        assert!(
            dir.path()
                .join("SingletonCookie")
                .symlink_metadata()
                .is_err()
        );

        std::os::unix::fs::symlink(format!("{host}-{}", std::process::id()), &lock).unwrap();
        let err = clear_stale_singleton(dir.path()).unwrap_err();
        assert_eq!(err.code(), "profile");
        assert!(lock.symlink_metadata().is_ok(), "live lock untouched");
    }

    #[test]
    fn foreign_host_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("other-host.example.com-1", dir.path().join("SingletonLock"))
            .unwrap();
        assert!(clear_stale_singleton(dir.path()).is_err());
    }

    #[test]
    fn no_lock_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        clear_stale_singleton(dir.path()).unwrap();
    }
}
