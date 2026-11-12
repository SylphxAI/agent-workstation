//! The desktop's VNC server: wayvnc on a unix socket in the session's
//! private runtime directory. Only the takeover bridge connects to it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

use crate::error::{Error, Result};

/// A running VNC server.
#[derive(Debug)]
pub struct Vnc {
    child: Child,
    socket: PathBuf,
}

impl Vnc {
    /// Start `binary` (usually `wayvnc`) for the desktop `desktop_env` names.
    pub async fn start(binary: &Path, desktop_env: &[(String, String)], runtime_dir: &Path) -> Result<Self> {
        let socket = runtime_dir.join("vnc.sock");
        if let Err(e) = std::fs::remove_file(&socket)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }
        let log = std::fs::File::create(runtime_dir.join("vnc.log"))?;
        let mut child = Command::new(binary)
            .arg("--unix-socket")
            .arg("--render-cursor")
            .arg(&socket)
            .envs(desktop_env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|e| Error::Process(format!("cannot start {}: {e}", binary.display())))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Process(format!("VNC server exited: {status}")));
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                return Err(Error::Timeout("VNC socket".into()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self { child, socket })
    }

    /// Unix socket speaking RFB.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop the server.
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = std::fs::remove_file(&self.socket);
    }
}
