//! Xvfb + window manager.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::process::{Child, Command};

use super::{Display, DisplayBackend};
use crate::error::{Error, Result};

/// First display number tried.
const FIRST_DISPLAY: u32 = 90;
/// Display numbers tried before giving up.
const DISPLAY_ATTEMPTS: u32 = 400;
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Window manager run on the display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WindowManager {
    /// `openbox --sm-disable`. Default.
    #[default]
    Openbox,
    /// No window manager.
    None,
    /// A caller-chosen command line.
    Command(Vec<String>),
}

/// The Xvfb backend.
#[derive(Clone, Debug)]
pub struct Xvfb {
    binary: PathBuf,
    window_manager: WindowManager,
    runtime_dir: PathBuf,
}

impl Xvfb {
    /// Xvfb from PATH with openbox; authority files under `runtime_dir`.
    pub fn new(runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from("Xvfb"),
            window_manager: WindowManager::Openbox,
            runtime_dir: runtime_dir.into(),
        }
    }

    /// Use a specific Xvfb binary.
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Choose the window manager.
    pub fn with_window_manager(mut self, wm: WindowManager) -> Self {
        self.window_manager = wm;
        self
    }

    fn free_display(n: u32) -> bool {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    }
}

#[async_trait]
impl DisplayBackend for Xvfb {
    async fn start(&self, width: u32, height: u32) -> Result<Box<dyn Display>> {
        if !(320..=7680).contains(&width) || !(240..=4320).contains(&height) {
            return Err(Error::InvalidInput(format!(
                "display size {width}x{height} is out of range"
            )));
        }
        std::fs::create_dir_all(&self.runtime_dir)?;
        let mut last_err = None;
        for n in FIRST_DISPLAY..FIRST_DISPLAY + DISPLAY_ATTEMPTS {
            if !Self::free_display(n) {
                continue;
            }
            match self.try_start(n, width, height).await {
                Ok(d) => return Ok(Box::new(d)),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Process("no free X display number".into())))
    }
}

impl Xvfb {
    async fn try_start(&self, n: u32, width: u32, height: u32) -> Result<XvfbDisplay> {
        let name = format!(":{n}");
        let auth = self.runtime_dir.join(format!("xauth-{n}"));
        write_xauthority(&auth, &random_cookie()?)?;
        let mut server = Command::new(&self.binary)
            .arg(&name)
            .args(["-screen", "0", &format!("{width}x{height}x24")])
            .args(["-auth"])
            .arg(&auth)
            .args([
                "-nolisten",
                "tcp",
                "-dpi",
                "96",
                "-noreset",
                "+extension",
                "RANDR",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|e| Error::Process(format!("cannot start Xvfb: {e}")))?;
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(status) = server.try_wait()? {
                let _ = std::fs::remove_file(&auth);
                return Err(Error::Process(format!("Xvfb {name} exited: {status}")));
            }
            if socket.exists() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = server.kill().await;
                let _ = std::fs::remove_file(&auth);
                return Err(Error::Timeout(format!("Xvfb {name} socket")));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let wm_cmd: Option<Vec<String>> = match &self.window_manager {
            WindowManager::Openbox => Some(vec!["openbox".into(), "--sm-disable".into()]),
            WindowManager::None => None,
            WindowManager::Command(c) if c.is_empty() => {
                return Err(Error::InvalidInput("empty window manager command".into()));
            }
            WindowManager::Command(c) => Some(c.clone()),
        };
        let wm = match wm_cmd {
            None => None,
            Some(cmd) => {
                let spawned = Command::new(&cmd[0])
                    .args(&cmd[1..])
                    .env("DISPLAY", &name)
                    .env("XAUTHORITY", &auth)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .process_group(0)
                    .spawn();
                match spawned {
                    Ok(c) => Some(c),
                    Err(e) => {
                        let _ = server.kill().await;
                        let _ = std::fs::remove_file(&auth);
                        return Err(Error::Process(format!(
                            "cannot start window manager {}: {e}",
                            cmd[0]
                        )));
                    }
                }
            }
        };
        Ok(XvfbDisplay {
            name,
            auth,
            size: (width, height),
            server,
            wm,
        })
    }
}

/// A running Xvfb display.
#[derive(Debug)]
pub struct XvfbDisplay {
    name: String,
    auth: PathBuf,
    size: (u32, u32),
    server: Child,
    wm: Option<Child>,
}

#[async_trait]
impl Display for XvfbDisplay {
    fn name(&self) -> &str {
        &self.name
    }

    fn xauthority(&self) -> Option<&PathBuf> {
        Some(&self.auth)
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn is_running(&mut self) -> bool {
        matches!(self.server.try_wait(), Ok(None))
    }

    async fn shutdown(mut self: Box<Self>) -> Result<()> {
        if let Some(mut wm) = self.wm.take() {
            let _ = wm.kill().await;
        }
        let _ = self.server.kill().await;
        let _ = std::fs::remove_file(&self.auth);
        Ok(())
    }
}

fn random_cookie() -> Result<[u8; 16]> {
    let mut cookie = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut cookie)?;
    Ok(cookie)
}

/// Write an Xauthority file with one FamilyWild MIT-MAGIC-COOKIE-1 entry
/// (matches any host for this display), mode 0600.
fn write_xauthority(path: &Path, cookie: &[u8; 16]) -> Result<()> {
    fn field(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0xffffu16.to_be_bytes());
    field(&mut out, b"");
    field(&mut out, b"");
    field(&mut out, b"MIT-MAGIC-COOKIE-1");
    field(&mut out, cookie);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(&out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xauthority_entry_is_family_wild_with_a_16_byte_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth");
        write_xauthority(&p, &[7u8; 16]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..2], &[0xff, 0xff]);
        assert_eq!(b.len(), 2 + 2 + 2 + 2 + 18 + 2 + 16);
        assert_eq!(&b[b.len() - 16..], &[7u8; 16]);
        assert_ne!(random_cookie().unwrap(), random_cookie().unwrap());
    }

    #[tokio::test]
    async fn out_of_range_sizes_are_refused_before_starting_anything() {
        let x = Xvfb::new(std::env::temp_dir());
        assert_eq!(x.start(10, 10).await.unwrap_err().code(), "invalid-input");
    }
}
