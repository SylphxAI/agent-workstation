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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::error::{Error, Result};

const START_TIMEOUT: Duration = Duration::from_secs(15);

/// A running headless compositor.
#[derive(Debug)]
pub struct Compositor {
    child: Child,
    runtime_dir: PathBuf,
    wayland_display: String,
    ipc_socket: PathBuf,
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
        let ipc_socket = runtime_dir.join("sway.sock");
        // sway falls back to a different socket when SWAYSOCK already exists.
        // A runtime directory must not be shared with another compositor.
        if ipc_socket.try_exists()? {
            return Err(Error::InvalidInput(
                "compositor IPC socket already exists".into(),
            ));
        }
        let config = runtime_dir.join("compositor.conf");
        std::fs::write(&config, config_text(width, height))?;
        let log = std::fs::File::create(runtime_dir.join("compositor.log"))?;
        let mut child = Command::new(binary)
            .arg("--config")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("SWAYSOCK", &ipc_socket)
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
            if let Some(name) = find_socket(runtime_dir).filter(|_| ipc_socket.exists()) {
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
            ipc_socket,
            size: (width, height),
        })
    }

    /// Wait until the launched process has a mapped, keyboard-focused window.
    /// CDP availability does not imply that a Wayland surface can receive input.
    pub(crate) async fn wait_for_focused_window(&self, pid: u32) -> Result<()> {
        wait_for_focus(&self.ipc_socket, pid, START_TIMEOUT).await
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

// Subscribe before reading the tree so a map/focus transition cannot be lost
// between the initial query and waiting. Sway only puts mapped windows in its
// tree; `focused` is the seat's keyboard focus, not an activated CDP target.
async fn wait_for_focus(socket: &Path, pid: u32, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, observe_focus(socket, pid))
        .await
        .map_err(|_| Error::Timeout("browser window keyboard focus".into()))?
}

async fn observe_focus(socket: &Path, pid: u32) -> Result<()> {
    let mut stream = UnixStream::connect(socket).await?;
    ipc_send(&mut stream, 2, br#"["window"]"#).await?;
    let (subscription, _) = ipc_reply(&mut stream, 2).await?;
    if subscription
        .get("success")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(Error::Protocol(
            "compositor refused window subscription".into(),
        ));
    }
    loop {
        ipc_send(&mut stream, 4, b"").await?; // GET_TREE
        let (tree, changed) = ipc_reply(&mut stream, 4).await?;
        if focused_process(&tree, pid) {
            return Ok(());
        }
        if changed {
            // An event preceding the reply may be newer than its tree snapshot.
            // Requery rather than discarding that wakeup and waiting forever.
            continue;
        }
        // Event-driven wait, not a launch delay or input retry. A subsequent
        // tree query checks the full predicate, including the process identity.
        while ipc_read(&mut stream).await?.0 != 0x8000_0003 {}
    }
}

fn focused_process(node: &serde_json::Value, pid: u32) -> bool {
    if node.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
        && node.get("focused").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return true;
    }
    ["nodes", "floating_nodes"].iter().any(|key| {
        node.get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|nodes| nodes.iter().any(|n| focused_process(n, pid)))
    })
}

async fn ipc_send(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> Result<()> {
    let mut header = Vec::from(*b"i3-ipc");
    header.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    header.extend_from_slice(&kind.to_ne_bytes());
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn ipc_read(stream: &mut UnixStream) -> Result<(u32, serde_json::Value)> {
    let mut header = [0; 14];
    stream.read_exact(&mut header).await?;
    if &header[..6] != b"i3-ipc" {
        return Err(Error::Protocol("invalid compositor IPC header".into()));
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    // Bound allocation even if a broken peer advertises an arbitrary length.
    if len > 16 * 1024 * 1024 {
        return Err(Error::Protocol(
            "compositor IPC reply exceeds 16 MiB".into(),
        ));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    let value = serde_json::from_slice(&payload)
        .map_err(|e| Error::Protocol(format!("compositor IPC JSON: {e}")))?;
    Ok((kind, value))
}

async fn ipc_reply(stream: &mut UnixStream, expected: u32) -> Result<(serde_json::Value, bool)> {
    let mut changed = false;
    loop {
        let (kind, value) = ipc_read(stream).await?;
        if kind == expected {
            return Ok((value, changed));
        }
        changed |= kind == 0x8000_0003;
        if kind & 0x8000_0000 == 0 {
            return Err(Error::Protocol("unexpected compositor IPC reply".into()));
        }
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

    #[test]
    fn focus_requires_the_launched_process_including_floating_windows() {
        use serde_json::json;
        assert!(focused_process(
            &json!({"floating_nodes": [
                {"pid": 42, "focused": true}
            ]}),
            42
        ));
        for tree in [
            json!({"nodes": [{"pid": 42, "focused": false}]}),
            json!({"nodes": [{"pid": 43, "focused": true}]}),
            json!({"nodes": [{"focused": true}]}),
            json!({"nodes": []}),
            json!({"pid": 42, "focused": "true"}),
        ] {
            assert!(!focused_process(&tree, 42), "{tree}");
        }
    }

    async fn request(stream: &mut UnixStream, kind: u32, payload: &[u8]) {
        let mut header = [0; 14];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(&header[..6], b"i3-ipc");
        assert_eq!(u32::from_ne_bytes(header[10..].try_into().unwrap()), kind);
        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut received = vec![0; len];
        stream.read_exact(&mut received).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn readiness_subscribes_first_and_rechecks_focus_after_events() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("ipc.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream, 2, br#"["window"]"#).await;
            ipc_send(&mut stream, 2, br#"{"success":true}"#)
                .await
                .unwrap();
            request(&mut stream, 4, b"").await;
            ipc_send(&mut stream, 4, br#"{"nodes":[{"pid":42,"focused":false}]}"#)
                .await
                .unwrap();
            ipc_send(&mut stream, 0x8000_0003, br#"{"change":"focus"}"#)
                .await
                .unwrap();
            request(&mut stream, 4, b"").await;
            ipc_send(&mut stream, 4, br#"{"nodes":[{"pid":43,"focused":true}]}"#)
                .await
                .unwrap();
            ipc_send(&mut stream, 0x8000_0003, br#"{"change":"new"}"#)
                .await
                .unwrap();
            request(&mut stream, 4, b"").await;
            // A queued event may be newer than the tree reply. Do not lose
            // it and wait for another event that will never arrive.
            ipc_send(&mut stream, 0x8000_0003, br#"{"change":"focus"}"#)
                .await
                .unwrap();
            ipc_send(&mut stream, 4, br#"{"nodes":[]}"#).await.unwrap();
            request(&mut stream, 4, b"").await;
            ipc_send(&mut stream, 4, br#"{"nodes":[{"pid":42,"focused":true}]}"#)
                .await
                .unwrap();
        });
        wait_for_focus(&path, 42, Duration::from_secs(2))
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_window_times_out_without_retrying_input() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("ipc.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream, 2, br#"["window"]"#).await;
            ipc_send(&mut stream, 2, br#"{"success":true}"#)
                .await
                .unwrap();
            request(&mut stream, 4, b"").await;
            ipc_send(&mut stream, 4, br#"{"nodes":[]}"#).await.unwrap();
            // No window/focus event. Hold the socket until the client times out.
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let err = wait_for_focus(&path, 42, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_ipc_fails_closed() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(&[0; 14]).await.unwrap();
        assert!(matches!(
            ipc_read(&mut client).await,
            Err(Error::Protocol(_))
        ));
        let (mut client, mut server) = UnixStream::pair().unwrap();
        ipc_send(&mut server, 2, b"not json").await.unwrap();
        assert!(matches!(
            ipc_read(&mut client).await,
            Err(Error::Protocol(_))
        ));
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
