//! One desktop session.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::browser::{Browser, BrowserConfig, Snapshot, Tab, Web};
use crate::compositor::Compositor;
use crate::error::{Error, Result};
use crate::fingerprint::{Fingerprint, FingerprintDefaults};
use crate::gate::{ControlGate, HandBack};
use crate::profile::Profile;
use crate::proxy::{Proxy, ProxyEndpoint};
use crate::secret::SecretResolver;
use crate::takeover::{Takeover, TakeoverConfig};
use crate::vnc::Vnc;
use crate::wayland::{self, Button, Frame, Input, InputDevice};

/// Everything a session needs; all paths and values are caller-owned.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Persistent browser profile.
    pub profile: Profile,
    /// Values for a fingerprint created on first use of the profile.
    pub fingerprint_defaults: FingerprintDefaults,
    /// Browser binary and flags.
    pub browser: BrowserConfig,
    /// Egress proxy.
    pub proxy: Option<Proxy>,
    /// Private, per-session runtime directory (sockets and logs).
    pub runtime_dir: PathBuf,
    /// Compositor binary.
    pub compositor: PathBuf,
    /// VNC server binary.
    pub vnc: PathBuf,
    /// Serve the takeover page; `None` disables takeover.
    pub takeover: Option<TakeoverConfig>,
}

impl SessionConfig {
    /// Defaults: `sway`, `wayvnc`, takeover on loopback.
    pub fn new(profile: Profile, browser: BrowserConfig, runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            profile,
            fingerprint_defaults: FingerprintDefaults::default(),
            browser,
            proxy: None,
            runtime_dir: runtime_dir.into(),
            compositor: "sway".into(),
            vnc: "wayvnc".into(),
            takeover: Some(TakeoverConfig::default()),
        }
    }
}

/// One headless Wayland desktop with one headful browser.
///
/// Web and desktop operations act on the same window and share one control
/// gate: while a person holds control, every call returns
/// [`Error::HumanInControl`].
#[derive(Debug)]
pub struct DesktopSession {
    gate: Arc<ControlGate>,
    fingerprint: Fingerprint,
    web: Mutex<Web>,
    takeover: Option<Takeover>,
    vnc: Option<Vnc>,
    browser: Option<Browser>,
    compositor: Option<Compositor>,
    _proxy: Option<ProxyEndpoint>,
    socket: PathBuf,
    input: Arc<std::sync::Mutex<InputDevice>>,
    size: (u32, u32),
}

impl DesktopSession {
    /// Start the compositor, the browser, the VNC server and (when
    /// configured) the takeover page. Anything started is stopped again if a
    /// later step fails.
    pub async fn start(config: SessionConfig) -> Result<Self> {
        let fingerprint = config.profile.fingerprint(&config.fingerprint_defaults)?;
        fingerprint.validate()?;
        let size = (fingerprint.screen_width, fingerprint.screen_height);
        let compositor =
            Compositor::start(&config.compositor, &config.runtime_dir, size.0, size.1).await?;
        let env = compositor.client_env();
        let socket = compositor.socket_path();
        let proxy = match &config.proxy {
            Some(p) => Some(p.start().await?),
            None => None,
        };
        // Register the seat's keyboard/pointer before Chromium binds wl_seat,
        // rather than hoping it has processed a capability change after a delay.
        let input = {
            let (socket, size) = (socket.clone(), size);
            match blocking(move || InputDevice::open(&socket, size)).await {
                Ok(i) => Arc::new(std::sync::Mutex::new(i)),
                Err(e) => {
                    compositor.shutdown().await;
                    return Err(e);
                }
            }
        };
        let log = config.runtime_dir.join("browser.log");
        let browser = match Browser::launch(
            &config.browser,
            &env,
            &config.profile.user_data_dir(),
            &fingerprint,
            proxy.as_ref().map(|p| p.browser_url.as_str()),
            &log,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                compositor.shutdown().await;
                return Err(e);
            }
        };
        if let Err(e) = async { compositor.wait_for_focused_window(browser.pid()?).await }.await {
            browser.shutdown().await;
            compositor.shutdown().await;
            return Err(e);
        }
        let mut session = Self {
            gate: ControlGate::new(),
            fingerprint,
            web: Mutex::new(match Web::connect(browser.endpoint()).await {
                Ok(w) => w,
                Err(e) => {
                    browser.shutdown().await;
                    compositor.shutdown().await;
                    return Err(e);
                }
            }),
            takeover: None,
            vnc: None,
            browser: Some(browser),
            compositor: Some(compositor),
            _proxy: proxy,
            socket,
            input,
            size,
        };
        if let Some(tc) = config.takeover {
            let started = async {
                let vnc = Vnc::start(&config.vnc, &env, &config.runtime_dir).await?;
                let t = Takeover::start(tc, vnc.socket().to_path_buf(), session.gate.clone()).await;
                Ok::<_, Error>((vnc, t))
            }
            .await;
            match started {
                Ok((vnc, Ok(t))) => {
                    session.vnc = Some(vnc);
                    session.takeover = Some(t);
                }
                Ok((vnc, Err(e))) => {
                    vnc.shutdown().await;
                    session.shutdown().await;
                    return Err(e);
                }
                Err(e) => {
                    session.shutdown().await;
                    return Err(e);
                }
            }
        }
        Ok(session)
    }

    /// The control gate (for consumers that surface takeover state).
    pub fn gate(&self) -> &Arc<ControlGate> {
        &self.gate
    }

    /// Takeover page URL (with its token), when takeover is enabled.
    pub fn takeover_url(&self) -> Option<url::Url> {
        self.takeover.as_ref().map(Takeover::url)
    }

    /// The profile's fingerprint.
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// Desktop size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The most recent hand-back from a person (with their note), once.
    pub fn take_hand_back(&self) -> Option<HandBack> {
        self.gate.take_hand_back()
    }

    async fn web(&self) -> Result<tokio::sync::MutexGuard<'_, Web>> {
        self.gate.check_agent()?;
        let guard = self.web.lock().await;
        // A person may have taken control while this call waited.
        self.gate.check_agent()?;
        Ok(guard)
    }

    // ---- web operations -------------------------------------------------

    /// Navigate the active tab; returns the final URL.
    pub async fn navigate(&self, url: &str) -> Result<String> {
        self.web().await?.navigate(url).await
    }

    /// History back.
    pub async fn back(&self) -> Result<String> {
        self.web().await?.history(-1).await
    }

    /// History forward.
    pub async fn forward(&self) -> Result<String> {
        self.web().await?.history(1).await
    }

    /// Reload.
    pub async fn reload(&self) -> Result<String> {
        self.web().await?.reload().await
    }

    /// URL of the active tab.
    pub async fn current_url(&self) -> Result<String> {
        self.web().await?.current_url().await
    }

    /// Open tabs.
    pub async fn tabs(&self) -> Result<Vec<Tab>> {
        self.web().await?.tabs().await
    }

    /// Open and activate a tab.
    pub async fn new_tab(&self, url: &str) -> Result<String> {
        self.web().await?.new_tab(url).await
    }

    /// Activate a tab.
    pub async fn select_tab(&self, id: &str) -> Result<()> {
        self.web().await?.select_tab(id).await
    }

    /// Close a tab.
    pub async fn close_tab(&self, id: &str) -> Result<()> {
        self.web().await?.close_tab(id).await
    }

    /// Accessibility snapshot with `@eN` refs.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.web().await?.snapshot().await
    }

    /// Visible page text.
    pub async fn read(&self) -> Result<String> {
        self.web().await?.read().await
    }

    /// Click an element by ref.
    pub async fn click(&self, reference: &str) -> Result<()> {
        self.web().await?.click(reference).await
    }

    /// Replace a field's value.
    pub async fn fill(&self, reference: &str, text: &str) -> Result<()> {
        self.web().await?.fill(reference, text).await
    }

    /// Fill a field with the secret `secret_ref` names, resolved by
    /// `resolver` at the moment of insertion. When `expect_origin` is given,
    /// the fill is refused unless the active tab is on that origin.
    pub async fn fill_secret(
        &self,
        reference: &str,
        secret_ref: &str,
        resolver: &dyn SecretResolver,
        expect_origin: Option<&str>,
    ) -> Result<()> {
        let mut web = self.web().await?;
        let secret = resolver.resolve(secret_ref).await?;
        self.gate.check_agent()?;
        web.fill_secret(reference, &secret, expect_origin).await
    }

    // ---- desktop operations ---------------------------------------------

    /// Screenshot of the whole desktop.
    pub async fn screenshot(&self) -> Result<Frame> {
        self.gate.check_agent()?;
        let socket = self.socket.clone();
        let frame = blocking(move || wayland::screenshot(&socket)).await?;
        self.gate.check_agent()?;
        Ok(frame)
    }

    /// Run pointer/keyboard steps on the desktop.
    pub async fn input(&self, steps: Vec<Input>) -> Result<()> {
        self.gate.check_agent()?;
        let _web = self.web.lock().await;
        self.gate.check_agent()?;
        let input = self.input.clone();
        blocking(move || input.lock().map_err(|_| Error::Closed)?.run(&steps)).await
    }

    /// Click at desktop coordinates.
    pub async fn click_at(&self, x: f64, y: f64, button: Button) -> Result<()> {
        self.input(vec![
            Input::Move { x, y },
            Input::Button {
                button,
                pressed: true,
            },
            Input::Button {
                button,
                pressed: false,
            },
        ])
        .await
    }

    /// Press a key chord such as `ctrl+l`.
    pub async fn key(&self, chord: &str) -> Result<()> {
        self.input(vec![Input::Key(chord.to_string())]).await
    }

    /// Type text into whatever has keyboard focus.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        self.input(vec![Input::Text(text.to_string())]).await
    }

    /// Scroll at desktop coordinates by notches.
    pub async fn scroll(&self, x: f64, y: f64, dx: i32, dy: i32) -> Result<()> {
        self.input(vec![Input::Move { x, y }, Input::Scroll { dx, dy }])
            .await
    }

    /// Stop everything. The profile is flushed by the browser's graceful close.
    pub async fn shutdown(mut self) {
        if let Some(t) = self.takeover.take() {
            t.shutdown().await;
        }
        if let Some(v) = self.vnc.take() {
            v.shutdown().await;
        }
        if let Some(b) = self.browser.take() {
            b.shutdown().await;
        }
        if let Some(c) = self.compositor.take() {
            c.shutdown().await;
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Process(format!("desktop worker: {e}")))?
}
