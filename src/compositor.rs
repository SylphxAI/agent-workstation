//! The session's headless Wayland compositor.
//!
//! sway runs with the headless wlroots backend and the software renderer, in
//! a private runtime directory (mode 0700) that holds the Wayland socket, the
//! compositor's IPC socket and the VNC socket. No X server, no Xwayland, no
//! TCP listener. One output at the session's resolution; the browser window
//! tiles to fill it without borders, so its native chrome is the top of the
//! desktop.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

use crate::error::{Error, Result};

const START_TIMEOUT: Duration = Duration::from_secs(15);

/// A running headless compositor.
#[derive(Debug)]
pub struct Compositor {
    child: Child,
    runtime_dir: PathBuf,
    wayland_display: String,
    size: (u32, u32),
}

impl Compositor {
    /// Start sway (`binary`, usually `sway`) at `width`×`height` with
    /// `runtime_dir` as its private `XDG_RUNTIME_DIR`. The directory is
    /// created with mode 0700 and must not be shared with another session.
    pub async fn start(binary: &Path, runtime_dir: &Path, width: u32, height: u32) -> Result<Self> {
        if !(320..=7680).contains(&width) || !(240..=4320).contains(&height) {
            return Err(Error::InvalidInput(format!(
                "desktop size {width}x{height} is out of range"
            )));
        }
        create_private_dir(runtime_dir)?;
        let config = runtime_dir.join("compositor.conf");
        std::fs::write(&config, config_text(width, height))?;
        let log = std::fs::File::create(runtime_dir.join("compositor.log"))?;
        let mut child = Command::new(binary)
            .arg("--config")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .envs(std::env::var_os("LD_LIBRARY_PATH").map(|v| ("LD_LIBRARY_PATH", v)))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|e| {
                Error::Process(format!("cannot start compositor {}: {e}", binary.display()))
            })?;
        let deadline = Instant::now() + START_TIMEOUT;
        let wayland_display = loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Process(format!(
                    "compositor exited during start-up ({status}); see {}",
                    runtime_dir.join("compositor.log").display()
                )));
            }
            if let Some(name) = find_socket(runtime_dir) {
                break name;
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                return Err(Error::Timeout("compositor socket".into()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        Ok(Self {
            child,
            runtime_dir: runtime_dir.to_path_buf(),
            wayland_display,
            size: (width, height),
        })
    }

    /// Private runtime directory.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Wayland socket name inside the runtime directory, e.g. `wayland-1`.
    pub fn wayland_display(&self) -> &str {
        &self.wayland_display
    }

    /// Absolute Wayland socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join(&self.wayland_display)
    }

    /// Output size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Environment a client needs to open windows on this desktop.
    pub fn client_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "XDG_RUNTIME_DIR".into(),
                self.runtime_dir.display().to_string(),
            ),
            ("WAYLAND_DISPLAY".into(), self.wayland_display.clone()),
        ]
    }

    /// Whether the compositor is alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop the compositor (every client on it loses its display).
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

fn config_text(width: u32, height: u32) -> String {
    format!(
        "xwayland disable\n\
         output HEADLESS-1 resolution {width}x{height} position 0 0 scale 1\n\
         default_border none\n\
         default_floating_border none\n\
         focus_follows_mouse no\n\
         seat * hide_cursor 0\n"
    )
}

fn find_socket(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix("wayland-")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_one_output_no_xwayland_no_borders() {
        let c = config_text(1366, 768);
        assert!(c.contains("xwayland disable"));
        assert!(c.contains("output HEADLESS-1 resolution 1366x768"));
        assert!(c.contains("default_border none"));
    }

    #[test]
    fn socket_discovery_ignores_locks_and_ipc() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "wayland-1.lock",
            "sway-ipc.1.sock",
            "wayland-x",
            "wayland-1",
        ] {
            std::fs::write(d.path().join(n), "").unwrap();
        }
        assert_eq!(find_socket(d.path()).as_deref(), Some("wayland-1"));
    }

    #[tokio::test]
    async fn out_of_range_size_is_refused_before_spawning() {
        let d = tempfile::tempdir().unwrap();
        let e = Compositor::start(Path::new("/nonexistent"), d.path(), 10, 10)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "invalid-input");
    }
}
