//! Chromium-family launcher (Chromium, Chrome, Edge).
//!
//! The browser runs headful on the session display with a persistent
//! `--user-data-dir`. The debugging port is chosen by the browser
//! (`--remote-debugging-port=0`) and read back from the `DevToolsActivePort`
//! file it writes into the profile, so no port is ever raced.
//!
//! The flag set is deliberately small: no `--enable-automation`, no
//! `--headless`, no JavaScript injection. Fingerprint values reach the browser
//! through the window size, `--accept-lang`, the locale environment and `TZ`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::process::{Child, Command};

use super::{BrowserProcess, Engine, EngineKind, LaunchSpec};
use crate::error::{Error, Result};
use crate::fingerprint::UserAgentMode;

/// How long a launch may take before the browser is killed.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
/// Grace period between a graceful close and a kill.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Launches Chromium-family browsers from one binary.
#[derive(Clone, Debug)]
pub struct Chromium {
    kind: EngineKind,
    binary: PathBuf,
    user_agent: UserAgentMode,
    sandbox: bool,
}

impl Chromium {
    /// A launcher for `binary`.
    pub fn new(kind: EngineKind, binary: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            binary: binary.into(),
            user_agent: UserAgentMode::Native,
            sandbox: true,
        }
    }

    /// Find a binary for `kind`: `AGENT_WORKSTATION_CHROMIUM` when set, then
    /// the usual install locations.
    pub fn discover(kind: EngineKind) -> Result<Self> {
        if let Some(custom) =
            std::env::var_os("AGENT_WORKSTATION_CHROMIUM").filter(|v| !v.is_empty())
        {
            return Ok(Self::new(kind, PathBuf::from(custom)));
        }
        let candidates: &[&str] = match kind {
            EngineKind::Chrome => &["google-chrome-stable", "google-chrome"],
            EngineKind::Edge => &["microsoft-edge-stable", "microsoft-edge"],
            _ => &[
                "chromium",
                "chromium-browser",
                "google-chrome-stable",
                "google-chrome",
            ],
        };
        candidates
            .iter()
            .find_map(|name| which(name))
            .map(|bin| Self::new(kind, bin))
            .ok_or_else(|| Error::Process(format!("no {kind} binary found on PATH")))
    }

    /// Choose whether the recorded user agent overrides the engine's own.
    pub fn with_user_agent(mut self, mode: UserAgentMode) -> Self {
        self.user_agent = mode;
        self
    }

    /// Disable Chromium's sandbox (needed when running as root in some
    /// containers). Visible to a person as an infobar; off by default.
    pub fn without_sandbox(mut self) -> Self {
        self.sandbox = false;
        self
    }

    /// The binary this launcher runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Command-line arguments for `spec` (exposed for inspection and tests).
    pub fn args(&self, spec: &LaunchSpec) -> Vec<String> {
        let fp = &spec.fingerprint;
        let mut args = vec![
            format!("--user-data-dir={}", spec.user_data_dir.display()),
            "--remote-debugging-port=0".to_string(),
            "--remote-debugging-address=127.0.0.1".to_string(),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--password-store=basic".to_string(),
            "--disable-blink-features=AutomationControlled".to_string(),
            "--disable-dev-shm-usage".to_string(),
            "--disable-background-timer-throttling".to_string(),
            "--disable-backgrounding-occluded-windows".to_string(),
            "--disable-renderer-backgrounding".to_string(),
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
            "--window-position=0,0".to_string(),
            format!("--window-size={},{}", fp.screen_width, fp.screen_height),
            format!("--accept-lang={}", fp.accept_language()),
            format!(
                "--lang={}",
                fp.languages.first().map(String::as_str).unwrap_or("en-US")
            ),
        ];
        if !self.sandbox {
            args.push("--no-sandbox".into());
        }
        if self.user_agent == UserAgentMode::FromFingerprint && !fp.user_agent.is_empty() {
            args.push(format!("--user-agent={}", fp.user_agent));
        }
        if let Some(proxy) = &spec.proxy {
            let mut server = proxy.url.clone();
            // Chromium takes credentials through an auth challenge, not the flag.
            let _ = server.set_username("");
            let _ = server.set_password(None);
            args.push(format!(
                "--proxy-server={}",
                server.as_str().trim_end_matches('/')
            ));
            if !proxy.bypass.is_empty() {
                args.push(format!("--proxy-bypass-list={}", proxy.bypass.join(";")));
            }
        }
        args.extend(spec.extra_args.iter().cloned());
        args.push(
            spec.start_url
                .clone()
                .unwrap_or_else(|| "about:blank".to_string()),
        );
        args
    }
}

#[async_trait]
impl Engine for Chromium {
    fn kind(&self) -> EngineKind {
        self.kind
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Box<dyn BrowserProcess>> {
        spec.fingerprint.validate()?;
        std::fs::create_dir_all(&spec.user_data_dir)?;
        crate::profile::clear_stale_singleton(&spec.user_data_dir)?;
        let port_file = spec.user_data_dir.join("DevToolsActivePort");
        match std::fs::remove_file(&port_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let mut cmd = Command::new(&self.binary);
        cmd.args(self.args(&spec))
            .envs(spec.display_env.iter().cloned())
            .envs(spec.fingerprint.launch_env())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        match &spec.log_path {
            Some(path) => {
                let log = std::fs::File::create(path)?;
                cmd.stdout(log.try_clone()?).stderr(log);
            }
            None => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Process(format!("cannot start {}: {e}", self.binary.display())))?;

        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        let endpoint = loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Process(format!(
                    "browser exited during start-up: {status}"
                )));
            }
            if let Some(ep) = read_port_file(&port_file)
                && tokio::net::TcpStream::connect(("127.0.0.1", ep.0))
                    .await
                    .is_ok()
            {
                break format!("ws://127.0.0.1:{}{}", ep.0, ep.1);
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                return Err(Error::Timeout(
                    "browser debugging endpoint did not appear".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        Ok(Box::new(ChromiumProcess {
            kind: self.kind,
            pid: child.id(),
            child,
            endpoint,
        }))
    }
}

fn read_port_file(path: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim().to_string();
    (path.starts_with("/devtools/browser/") && port != 0).then_some((port, path))
}

pub(crate) fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// A running Chromium-family browser.
#[derive(Debug)]
pub struct ChromiumProcess {
    kind: EngineKind,
    pid: Option<u32>,
    child: Child,
    endpoint: String,
}

#[async_trait]
impl BrowserProcess for ChromiumProcess {
    fn kind(&self) -> EngineKind {
        self.kind
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn cdp_endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn shutdown(mut self: Box<Self>) -> Result<()> {
        // Browser.close flushes cookies and storage to the profile.
        if let Ok(mut cdp) = crate::cdp::Cdp::connect(&self.endpoint).await {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                cdp.call("Browser.close", serde_json::json!({})),
            )
            .await;
        }
        match tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(_) => Ok(()),
            Err(_) => {
                self.child.kill().await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Proxy;
    use crate::fingerprint::{Fingerprint, FingerprintDefaults};

    fn spec() -> LaunchSpec {
        LaunchSpec {
            display_env: vec![("DISPLAY".into(), ":99".into())],
            user_data_dir: "/profiles/a/user-data".into(),
            fingerprint: Fingerprint::generate(
                "a",
                &FingerprintDefaults {
                    languages: vec!["ja-JP".into(), "ja".into()],
                    timezone: "Asia/Tokyo".into(),
                    screen_width: 1366,
                    screen_height: 768,
                },
            ),
            proxy: Some(Proxy {
                url: "http://u:secret@proxy.example.com:3128".parse().unwrap(),
                bypass: vec!["localhost".into()],
            }),
            start_url: None,
            extra_args: vec!["--custom".into()],
            log_path: None,
        }
    }

    #[test]
    fn launch_flags_carry_fingerprint_and_proxy_without_secrets_or_automation_flags() {
        let args = Chromium::new(EngineKind::Chromium, "/bin/true").args(&spec());
        let joined = args.join(" ");
        assert!(args.contains(&"--window-size=1366,768".to_string()));
        assert!(args.contains(&"--accept-lang=ja-JP,ja".to_string()));
        assert!(args.contains(&"--proxy-server=http://proxy.example.com:3128".to_string()));
        assert!(args.contains(&"--proxy-bypass-list=localhost".to_string()));
        assert!(
            !joined.contains("secret"),
            "proxy password never on the command line"
        );
        assert!(!joined.contains("--enable-automation"));
        assert!(!joined.contains("--headless"));
        assert!(!joined.contains("--no-sandbox"));
        assert!(!joined.contains("--user-agent"), "native UA by default");
        assert_eq!(args.last().unwrap(), "about:blank");
        assert!(args.contains(&"--custom".to_string()));
    }

    #[test]
    fn recorded_user_agent_applies_only_when_chosen() {
        let mut s = spec();
        s.fingerprint.user_agent = "Mozilla/5.0 (X11; Linux x86_64) Test/1".into();
        let args = Chromium::new(EngineKind::Chromium, "/bin/true")
            .with_user_agent(UserAgentMode::FromFingerprint)
            .args(&s);
        assert!(
            args.iter()
                .any(|a| a == "--user-agent=Mozilla/5.0 (X11; Linux x86_64) Test/1")
        );
    }

    #[test]
    fn port_file_is_parsed_strictly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("DevToolsActivePort");
        std::fs::write(&p, "9222\n/devtools/browser/abc\n").unwrap();
        assert_eq!(
            read_port_file(&p),
            Some((9222, "/devtools/browser/abc".into()))
        );
        std::fs::write(&p, "9222\n").unwrap();
        assert_eq!(read_port_file(&p), None);
        std::fs::write(&p, "0\n/devtools/browser/abc\n").unwrap();
        assert_eq!(read_port_file(&p), None);
    }
}
