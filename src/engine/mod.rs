//! Browser engines.
//!
//! [`EngineKind`] is the vocabulary a caller names an engine with (wire names
//! are lowercase; aliases and case resolve to the same engine). [`Engine`]
//! launches one headful browser on a session's display; the running process
//! is a [`BrowserProcess`]. Chromium is the default; Camoufox is opt-in.
//!
//! Engines never inject page JavaScript to alter what a site sees. The
//! fingerprint is applied at launch through flags, environment and
//! protocol-level emulation only.

mod camoufox;
mod chromium;

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;

pub use camoufox::Camoufox;
pub use chromium::Chromium;

use crate::error::Result;
use crate::fingerprint::Fingerprint;

/// A browser engine a caller may ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EngineKind {
    /// Google Chrome.
    Chrome,
    /// Chromium, the default.
    Chromium,
    /// Microsoft Edge.
    #[serde(rename = "msedge", alias = "edge")]
    Edge,
    /// Mozilla Firefox.
    Firefox,
    /// Camoufox, a Firefox build with engine-level fingerprint control.
    Camoufox,
    /// WebKit.
    Webkit,
    /// Apple Safari.
    Safari,
}

/// The protocol the browser view drives an engine with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Protocol {
    /// Chrome DevTools Protocol.
    Cdp,
    /// Firefox's Juggler protocol (Playwright's Firefox transport).
    Juggler,
    /// WebDriver BiDi.
    WebDriverBidi,
}

impl EngineKind {
    /// Every engine a caller may name.
    pub const ALL: [EngineKind; 7] = [
        EngineKind::Chrome,
        EngineKind::Chromium,
        EngineKind::Edge,
        EngineKind::Firefox,
        EngineKind::Camoufox,
        EngineKind::Webkit,
        EngineKind::Safari,
    ];

    /// The engine an unspecified request gets.
    pub const DEFAULT: EngineKind = EngineKind::Chromium;

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            EngineKind::Chrome => "chrome",
            EngineKind::Chromium => "chromium",
            EngineKind::Edge => "msedge",
            EngineKind::Firefox => "firefox",
            EngineKind::Camoufox => "camoufox",
            EngineKind::Webkit => "webkit",
            EngineKind::Safari => "safari",
        }
    }

    /// Other names that resolve to this engine.
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            EngineKind::Edge => &["edge", "microsoft-edge", "microsoft edge"],
            EngineKind::Chrome => &["google-chrome", "google chrome"],
            EngineKind::Firefox => &["mozilla-firefox", "mozilla firefox"],
            _ => &[],
        }
    }

    /// Parse a name; `None` when it names no engine.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == value || e.aliases().contains(&value.as_str()))
    }

    /// The automation protocol the engine speaks.
    pub const fn protocol(self) -> Protocol {
        match self {
            EngineKind::Chrome | EngineKind::Chromium | EngineKind::Edge => Protocol::Cdp,
            EngineKind::Firefox | EngineKind::Camoufox => Protocol::Juggler,
            EngineKind::Webkit | EngineKind::Safari => Protocol::WebDriverBidi,
        }
    }

    /// Whether the engine is driven over CDP (the browser view's protocol).
    pub const fn speaks_cdp(self) -> bool {
        matches!(self.protocol(), Protocol::Cdp)
    }
}

impl Default for EngineKind {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for EngineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EngineKind {
    type Err = UnknownEngine;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| UnknownEngine {
            wanted: value.trim().to_string(),
        })
    }
}

/// A name that is not one of [`EngineKind::ALL`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownEngine {
    /// What the caller wrote.
    pub wanted: String,
}

impl fmt::Display for UnknownEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let known: Vec<_> = EngineKind::ALL.iter().map(|e| e.as_str()).collect();
        write!(
            f,
            "`{}` is not a browser engine; choose one of: {}",
            self.wanted,
            known.join(", ")
        )
    }
}

impl std::error::Error for UnknownEngine {}

/// Proxy every browser request goes through. The URL is caller-supplied, e.g.
/// `http://user:pass@proxy.example.com:8080` or `socks5://127.0.0.1:1080`.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    /// Proxy URL including scheme; credentials, when present, are never logged.
    pub url: url::Url,
    /// Hosts that bypass the proxy (Chromium `--proxy-bypass-list` syntax).
    pub bypass: Vec<String>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut shown = self.url.clone();
        let _ = shown.set_password(None);
        let _ = shown.set_username("");
        f.debug_struct("Proxy")
            .field("url", &shown.as_str())
            .field("bypass", &self.bypass)
            .finish()
    }
}

/// Everything an engine needs to launch one browser.
#[derive(Clone, Debug)]
pub struct LaunchSpec {
    /// Environment that reaches the session display (`DISPLAY`, `XAUTHORITY`),
    /// as [`crate::display::Display::client_env`] returns it.
    pub display_env: Vec<(String, String)>,
    /// Caller-owned persistent profile directory (Chromium `--user-data-dir`).
    pub user_data_dir: PathBuf,
    /// Fingerprint seed applied at launch.
    pub fingerprint: Fingerprint,
    /// Optional egress proxy.
    pub proxy: Option<Proxy>,
    /// First page; `about:blank` when `None`.
    pub start_url: Option<String>,
    /// Extra engine arguments appended verbatim (caller policy).
    pub extra_args: Vec<String>,
    /// File the browser's stdout/stderr go to; discarded when `None`.
    pub log_path: Option<PathBuf>,
}

/// Launches browsers of one engine.
#[async_trait]
pub trait Engine: Send + Sync + fmt::Debug {
    /// Which engine this is.
    fn kind(&self) -> EngineKind;

    /// Launch one headful browser on the display `spec.display_env` names and wait until its
    /// automation endpoint answers.
    async fn launch(&self, spec: LaunchSpec) -> Result<Box<dyn BrowserProcess>>;
}

/// A running browser.
#[async_trait]
pub trait BrowserProcess: Send + Sync + fmt::Debug {
    /// Engine of this browser.
    fn kind(&self) -> EngineKind;

    /// OS process id of the browser's main process.
    fn pid(&self) -> Option<u32>;

    /// Browser-level CDP WebSocket URL (`ws://127.0.0.1:<port>/devtools/browser/<id>`)
    /// when the engine speaks CDP.
    fn cdp_endpoint(&self) -> Option<&str>;

    /// Whether the process is still alive.
    fn is_running(&mut self) -> bool;

    /// Close the browser gracefully, then kill it after a grace period. Profile
    /// data is flushed by the graceful close.
    async fn shutdown(self: Box<Self>) -> Result<()>;
}

/// Build the engine a [`EngineKind`] names with default binary discovery.
pub fn engine_for(kind: EngineKind) -> Result<Box<dyn Engine>> {
    match kind {
        EngineKind::Chromium | EngineKind::Chrome | EngineKind::Edge => {
            Ok(Box::new(Chromium::discover(kind)?))
        }
        EngineKind::Camoufox => Ok(Box::new(Camoufox::discover()?)),
        other => Err(crate::Error::Unsupported(format!(
            "engine `{other}` has no launcher in this build"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_engine_round_trips_on_the_wire() {
        for engine in EngineKind::ALL {
            assert_eq!(EngineKind::parse(engine.as_str()), Some(engine));
            assert_eq!(engine.as_str().parse::<EngineKind>().unwrap(), engine);
            let json = serde_json::to_string(&engine).unwrap();
            assert_eq!(json, format!("\"{}\"", engine.as_str()));
            assert_eq!(serde_json::from_str::<EngineKind>(&json).unwrap(), engine);
        }
    }

    #[test]
    fn aliases_and_case_resolve_and_unknown_names_list_known_ones() {
        assert_eq!(
            EngineKind::parse(" Microsoft Edge "),
            Some(EngineKind::Edge)
        );
        assert_eq!(EngineKind::parse("CAMOUFOX"), Some(EngineKind::Camoufox));
        let err = "chromiun".parse::<EngineKind>().unwrap_err();
        assert!(err.to_string().contains("camoufox"));
        assert_eq!(EngineKind::parse(""), None);
        assert_eq!(EngineKind::default(), EngineKind::Chromium);
    }

    #[test]
    fn only_blink_engines_speak_cdp() {
        assert!(EngineKind::Chromium.speaks_cdp());
        assert!(EngineKind::Edge.speaks_cdp());
        assert!(!EngineKind::Camoufox.speaks_cdp());
        assert_eq!(EngineKind::Camoufox.protocol(), Protocol::Juggler);
    }

    #[test]
    fn proxy_debug_hides_credentials() {
        let p = Proxy {
            url: "http://user:hunter2@proxy.example.com:8080"
                .parse()
                .unwrap(),
            bypass: vec![],
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("user"));
    }
}
