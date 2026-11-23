//! Private D-Bus activation for one headless Wayland desktop.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{Error, Result};

#[derive(Debug)]
pub(crate) struct SessionBus {
    child: Child,
    socket: PathBuf,
    address: String,
}

impl SessionBus {
    pub(crate) async fn start(runtime: &Path, desktop_env: &[(String, String)]) -> Result<Self> {
        let socket = std::fs::canonicalize(runtime)?.join("dbus");
        // D-Bus addresses reserve separators and are not necessarily UTF-8 paths.
        let mut address = String::from("unix:path=");
        for &byte in socket.as_os_str().as_bytes() {
            if byte.is_ascii_alphanumeric() || b"_-/.*".contains(&byte) {
                address.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                let _ = write!(address, "%{byte:02x}");
            }
        }
        // No --systemd-activation: launch org.a11y.Bus in this environment,
        // never via the user's shared service manager. The compositor already
        // created a private XDG_RUNTIME_DIR; the launcher uses it for its bus.
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1", "--address"])
            .arg(&address)
            .envs(desktop_env.iter().cloned())
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env_remove("DISPLAY")
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env_remove("DBUS_STARTER_ADDRESS")
            .env_remove("DBUS_STARTER_BUS_TYPE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .map_err(|_| Error::Timeout("private D-Bus startup".into()))??;
        if ready.is_none() || !socket.exists() {
            return Err(Error::Process("private D-Bus failed to bind".into()));
        }
        Ok(Self {
            child,
            socket,
            address,
        })
    }

    pub(crate) fn address(&self) -> &str {
        &self.address
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}
