//! Camoufox launcher (opt-in).
//!
//! Camoufox is a Firefox build that applies fingerprint values inside the
//! engine from a JSON configuration passed in `CAMOU_CONFIG_<n>` environment
//! chunks. It speaks Juggler rather than CDP, so a Camoufox session offers the
//! computer view and full-display takeover; the CDP browser view reports
//! [`crate::Error::Unsupported`].
//!
//! The profile directory is a Firefox profile (`-profile <dir>`), distinct
//! from a Chromium `user-data` directory. The proxy is written to the
//! profile's `user.js` as Firefox preferences.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::{Child, Command};

use super::{BrowserProcess, Engine, EngineKind, LaunchSpec};
use crate::error::{Error, Result};

/// Maximum bytes per `CAMOU_CONFIG_<n>` chunk.
const CONFIG_CHUNK: usize = 2047;

/// Launches Camoufox.
#[derive(Clone, Debug)]
pub struct Camoufox {
    binary: PathBuf,
}

impl Camoufox {
    /// A launcher for `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// `AGENT_WORKSTATION_CAMOUFOX` when set, else `camoufox` / `camoufox-bin` on PATH.
    pub fn discover() -> Result<Self> {
        if let Some(custom) =
            std::env::var_os("AGENT_WORKSTATION_CAMOUFOX").filter(|v| !v.is_empty())
        {
            return Ok(Self::new(PathBuf::from(custom)));
        }
        ["camoufox", "camoufox-bin"]
            .iter()
            .find_map(|n| super::chromium::which(n))
            .map(Self::new)
            .ok_or_else(|| Error::Process("no camoufox binary found on PATH".into()))
    }

    /// Engine configuration derived from the fingerprint.
    pub fn config(spec: &LaunchSpec) -> serde_json::Value {
        let fp = &spec.fingerprint;
        json!({
            "screen.width": fp.screen_width,
            "screen.height": fp.screen_height,
            "screen.availWidth": fp.screen_width,
            "screen.availHeight": fp.screen_height,
            "window.outerWidth": fp.screen_width,
            "window.outerHeight": fp.screen_height,
            "navigator.language": fp.languages.first().cloned().unwrap_or_else(|| "en-US".into()),
            "navigator.languages": fp.languages,
            "headers.Accept-Language": fp.accept_language(),
            "timezone": fp.timezone,
        })
    }

    /// `CAMOU_CONFIG_<n>` environment chunks for `config`.
    pub fn config_env(config: &serde_json::Value) -> Vec<(String, String)> {
        let text = config.to_string();
        let mut out = Vec::new();
        let mut rest = text.as_str();
        let mut n = 1;
        while !rest.is_empty() {
            let mut cut = rest.len().min(CONFIG_CHUNK);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push((format!("CAMOU_CONFIG_{n}"), rest[..cut].to_string()));
            rest = &rest[cut..];
            n += 1;
        }
        out
    }

    /// Firefox preferences for `user.js`.
    pub fn user_prefs(spec: &LaunchSpec) -> String {
        let mut prefs = vec![
            (
                "browser.shell.checkDefaultBrowser".to_string(),
                json!(false),
            ),
            ("browser.aboutwelcome.enabled".to_string(), json!(false)),
            (
                "intl.accept_languages".to_string(),
                json!(spec.fingerprint.accept_language()),
            ),
        ];
        if let Some(proxy) = &spec.proxy {
            let host = proxy.url.host_str().unwrap_or_default().to_string();
            let port = proxy.url.port_or_known_default().unwrap_or(0);
            let socks = proxy.url.scheme().starts_with("socks");
            prefs.push(("network.proxy.type".into(), json!(1)));
            if socks {
                prefs.push(("network.proxy.socks".into(), json!(host)));
                prefs.push(("network.proxy.socks_port".into(), json!(port)));
                prefs.push(("network.proxy.socks_remote_dns".into(), json!(true)));
            } else {
                for k in ["http", "ssl"] {
                    prefs.push((format!("network.proxy.{k}"), json!(host)));
                    prefs.push((format!("network.proxy.{k}_port"), json!(port)));
                }
            }
            prefs.push((
                "network.proxy.no_proxies_on".into(),
                json!(proxy.bypass.join(",")),
            ));
        }
        prefs
            .into_iter()
            .map(|(k, v)| format!("user_pref({}, {v});\n", json!(k)))
            .collect()
    }
}

#[async_trait]
impl Engine for Camoufox {
    fn kind(&self) -> EngineKind {
        EngineKind::Camoufox
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Box<dyn BrowserProcess>> {
        spec.fingerprint.validate()?;
        std::fs::create_dir_all(&spec.user_data_dir)?;
        write_user_js(&spec.user_data_dir, &Self::user_prefs(&spec))?;
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-profile")
            .arg(&spec.user_data_dir)
            .arg("-no-remote")
            .args(&spec.extra_args)
            .arg(spec.start_url.as_deref().unwrap_or("about:blank"))
            .envs(spec.display_env.iter().cloned())
            .envs(spec.fingerprint.launch_env())
            .envs(Self::config_env(&Self::config(&spec)))
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
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(status) = child.try_wait()? {
            return Err(Error::Process(format!(
                "camoufox exited during start-up: {status}"
            )));
        }
        Ok(Box::new(CamoufoxProcess {
            pid: child.id(),
            child,
        }))
    }
}

fn write_user_js(profile: &Path, prefs: &str) -> Result<()> {
    std::fs::write(profile.join("user.js"), prefs)?;
    Ok(())
}

/// A running Camoufox.
#[derive(Debug)]
pub struct CamoufoxProcess {
    pid: Option<u32>,
    child: Child,
}

#[async_trait]
impl BrowserProcess for CamoufoxProcess {
    fn kind(&self) -> EngineKind {
        EngineKind::Camoufox
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn cdp_endpoint(&self) -> Option<&str> {
        None
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn shutdown(mut self: Box<Self>) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Proxy;
    use crate::fingerprint::{Fingerprint, FingerprintDefaults};

    fn spec() -> LaunchSpec {
        LaunchSpec {
            display_env: vec![("DISPLAY".into(), ":5".into())],
            user_data_dir: "/p".into(),
            fingerprint: Fingerprint::generate("a", &FingerprintDefaults::default()),
            proxy: Some(Proxy {
                url: "socks5://u:pw@127.0.0.1:1080".parse().unwrap(),
                bypass: vec![],
            }),
            start_url: Some("https://example.com/".into()),
            extra_args: vec![],
            log_path: None,
        }
    }

    #[test]
    fn config_chunks_reassemble_to_the_config() {
        let mut cfg = Camoufox::config(&spec());
        cfg["padding"] = json!("x".repeat(5000));
        let env = Camoufox::config_env(&cfg);
        assert!(env.len() >= 3);
        assert!(env.iter().all(|(_, v)| v.len() <= CONFIG_CHUNK));
        let joined: String = env.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
            cfg
        );
        assert_eq!(env[0].0, "CAMOU_CONFIG_1");
    }

    #[test]
    fn socks_proxy_becomes_prefs_without_credentials() {
        let prefs = Camoufox::user_prefs(&spec());
        assert!(prefs.contains(r#"user_pref("network.proxy.socks", "127.0.0.1");"#));
        assert!(prefs.contains(r#"user_pref("network.proxy.socks_port", 1080);"#));
        assert!(!prefs.contains("pw"));
    }
}
