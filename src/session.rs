//! One desktop session.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::browser::{Browser, BrowserConfig, Cookie, PageImage, Snapshot, Tab, Web};
#[cfg(target_os = "linux")]
use crate::compositor::Compositor;
use crate::error::{Error, Result};
use crate::fingerprint::{Fingerprint, FingerprintDefaults};
use crate::gate::{ControlGate, HandBack, Lease};
use crate::profile::Profile;
use crate::proxy::{Proxy, ProxyEndpoint};
use crate::secret::SecretResolver;
use crate::takeover::{Takeover, TakeoverConfig};
#[cfg(target_os = "linux")]
use crate::vnc::Vnc;
#[cfg(target_os = "linux")]
use crate::wayland::{self, InputDevice};
use crate::{Button, Frame, Input};

/// Longest key hold on the desktop or in a page.
pub const MAX_HOLD: Duration = crate::browser::MAX_HOLD;

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

/// The session's helper processes, owned until [`DesktopSession::close`].
#[derive(Debug, Default)]
struct Processes {
    #[cfg(target_os = "linux")]
    vnc: Option<Vnc>,
    browser: Option<Browser>,
    #[cfg(target_os = "linux")]
    compositor: Option<Compositor>,
    proxy: Option<ProxyEndpoint>,
}

/// One headless Wayland desktop with one headful browser.
///
/// One agent principal at a time owns it: [`DesktopSession::lease`] returns
/// that principal's [`Agent`] view, through which every web and desktop
/// operation goes, and refuses any other principal with [`Error::Leased`].
/// Web and desktop operations act on the same window and share one control
/// gate: while a person holds control, every call returns
/// [`Error::HumanInControl`].
#[derive(Debug)]
pub struct DesktopSession {
    gate: Arc<ControlGate>,
    fingerprint: Fingerprint,
    web: Mutex<Web>,
    takeover: Option<Takeover>,
    processes: std::sync::Mutex<Processes>,
    closed: AtomicBool,
    #[cfg(target_os = "linux")]
    socket: PathBuf,
    #[cfg(target_os = "linux")]
    input: Arc<std::sync::Mutex<InputDevice>>,
    size: (u32, u32),
}

impl DesktopSession {
    /// Start the compositor, the browser, the VNC server and (when
    /// configured) the takeover page. Anything started is stopped again if a
    /// later step fails.
    #[cfg(target_os = "linux")]
    pub async fn start(config: SessionConfig) -> Result<Self> {
        let fingerprint = config.profile.fingerprint(&config.fingerprint_defaults)?;
        fingerprint.validate()?;
        let size = (fingerprint.screen_width, fingerprint.screen_height);
        let gate = ControlGate::new();
        let compositor =
            Compositor::start(&config.compositor, &config.runtime_dir, size.0, size.1).await?;
        let env = compositor.client_env();
        let socket = compositor.socket_path();
        let proxy = match &config.proxy {
            Some(p) => match p.start().await {
                Ok(endpoint) => Some(endpoint),
                Err(e) => {
                    compositor.shutdown().await;
                    return Err(e);
                }
            },
            None => None,
        };
        // Register the seat's keyboard/pointer before Chromium binds wl_seat,
        // rather than hoping it has processed a capability change after a
        // delay. The pointer starts at the centre, so its position is known.
        let input = {
            let (socket, size) = (socket.clone(), size);
            let center = (f64::from(size.0 / 2), f64::from(size.1 / 2));
            let opened = blocking(move || {
                let mut device = InputDevice::open(&socket, size)?;
                device.run(&[Input::Move {
                    x: center.0,
                    y: center.1,
                }])?;
                Ok(device)
            })
            .await;
            match opened {
                Ok(i) => {
                    gate.note_pointer(center.0, center.1);
                    Arc::new(std::sync::Mutex::new(i))
                }
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
        let web = async {
            compositor.wait_for_focused_window(browser.pid()?).await?;
            Web::connect(browser.endpoint()).await
        }
        .await;
        let web = match web {
            Ok(w) => w,
            Err(e) => {
                browser.shutdown().await;
                compositor.shutdown().await;
                return Err(e);
            }
        };
        let mut processes = Processes {
            vnc: None,
            browser: Some(browser),
            compositor: Some(compositor),
            proxy,
        };
        let takeover = match config.takeover {
            None => None,
            Some(tc) => {
                let started = async {
                    let vnc = Vnc::start(&config.vnc, &env, &config.runtime_dir).await?;
                    match Takeover::start(tc, vnc.socket().to_path_buf(), gate.clone()).await {
                        Ok(t) => Ok((vnc, t)),
                        Err(e) => {
                            vnc.shutdown().await;
                            Err(e)
                        }
                    }
                }
                .await;
                match started {
                    Ok((vnc, t)) => {
                        processes.vnc = Some(vnc);
                        Some(t)
                    }
                    Err(e) => {
                        stop_processes(processes).await;
                        return Err(e);
                    }
                }
            }
        };
        Ok(Self {
            gate,
            fingerprint,
            web: Mutex::new(web),
            takeover,
            processes: std::sync::Mutex::new(processes),
            closed: AtomicBool::new(false),
            socket,
            input,
            size,
        })
    }

    /// Desktop sessions require Linux. No files or processes are created.
    #[cfg(not(target_os = "linux"))]
    pub async fn start(_config: SessionConfig) -> Result<Self> {
        Err(Error::Unsupported("desktop sessions require Linux".into()))
    }

    /// The control gate (for consumers that surface takeover state).
    pub fn gate(&self) -> &Arc<ControlGate> {
        &self.gate
    }

    /// Acquire the session for `principal` (or confirm it already holds it)
    /// and return its view. Another principal's lease is
    /// [`Error::Leased`], naming the holder.
    pub fn lease(&self, principal: &str) -> Result<Agent<'_>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        self.gate.acquire(principal)?;
        Ok(Agent {
            session: self,
            principal: principal.to_string(),
        })
    }

    /// Release `principal`'s lease, so another principal may acquire the
    /// session. `false` when it did not hold it. Held page keys are not
    /// released; the next holder starts with [`Agent::release_keys`] if it
    /// cares.
    pub fn release(&self, principal: &str) -> bool {
        self.gate.release_lease(principal)
    }

    /// Who holds the session, if anyone.
    pub fn leaseholder(&self) -> Option<Lease> {
        self.gate.lease()
    }

    /// Takeover page URL (with its token), when takeover is enabled.
    pub fn takeover_url(&self) -> Option<url::Url> {
        self.takeover.as_ref().map(Takeover::url)
    }

    /// Change where people reach the takeover page; see
    /// [`Takeover::set_public_url`].
    pub fn set_takeover_public_url(&self, url: url::Url) -> Result<()> {
        self.takeover
            .as_ref()
            .ok_or_else(|| Error::Unsupported("this session has no takeover page".into()))?
            .set_public_url(url)
    }

    /// The takeover page as a router to mount in the caller's own HTTP
    /// server; see [`Takeover::router`].
    pub fn takeover_router(&self) -> Option<axum::Router> {
        self.takeover.as_ref().map(Takeover::router)
    }

    /// The takeover page's own listener, when it has one.
    pub fn takeover_addr(&self) -> Option<std::net::SocketAddr> {
        self.takeover.as_ref().and_then(Takeover::local_addr)
    }

    /// The profile's fingerprint.
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// Desktop size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Whether [`DesktopSession::close`] has run.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Close the session: new calls fail with [`Error::Closed`], calls in
    /// flight finish first, then the takeover page, the VNC server, the
    /// browser (gracefully, so it flushes the profile) and the compositor
    /// stop. Safe to call from any holder of a shared reference, and more
    /// than once.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _in_flight = self.web.lock().await;
        if let Some(t) = &self.takeover {
            t.stop();
        }
        let processes =
            std::mem::take(&mut *self.processes.lock().unwrap_or_else(|p| p.into_inner()));
        stop_processes(processes).await;
    }

    fn check(&self, principal: &str) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        self.gate.check(principal)
    }
}

async fn stop_processes(mut p: Processes) {
    #[cfg(target_os = "linux")]
    if let Some(v) = p.vnc.take() {
        v.shutdown().await;
    }
    if let Some(b) = p.browser.take() {
        b.shutdown().await;
    }
    #[cfg(target_os = "linux")]
    if let Some(c) = p.compositor.take() {
        c.shutdown().await;
    }
    drop(p.proxy.take());
}

/// One agent principal's view of a [`DesktopSession`] it holds.
///
/// Every call first checks that the principal still holds the lease (another
/// principal's is [`Error::Leased`]) and that no person holds control
/// ([`Error::HumanInControl`]).
#[derive(Debug, Clone)]
pub struct Agent<'a> {
    session: &'a DesktopSession,
    principal: String,
}

impl<'a> Agent<'a> {
    /// The principal this view acts for.
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// The session.
    pub fn session(&self) -> &'a DesktopSession {
        self.session
    }

    /// Release the lease.
    pub fn release(self) -> bool {
        self.session.release(&self.principal)
    }

    /// The most recent hand-back from a person (with their note), once.
    pub fn take_hand_back(&self) -> Result<Option<HandBack>> {
        self.session.check(&self.principal)?;
        Ok(self.session.gate.take_hand_back())
    }

    async fn web(&self) -> Result<tokio::sync::MutexGuard<'a, Web>> {
        self.session.check(&self.principal)?;
        let guard = self.session.web.lock().await;
        // The lease, a person, or close may have changed while this call waited.
        self.session.check(&self.principal)?;
        Ok(guard)
    }

    // ---- web operations -------------------------------------------------

    /// Navigate the active tab; returns the final URL. Keyboard focus moves
    /// into the page.
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

    /// Close a tab; returns once the browser has destroyed it.
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

    /// Click an element by ref or CSS selector.
    pub async fn click(&self, target: &str) -> Result<()> {
        self.web().await?.click(target).await
    }

    /// Replace a field's value.
    pub async fn fill(&self, target: &str, text: &str) -> Result<()> {
        self.web().await?.fill(target, text).await
    }

    /// Fill a field with the secret `secret_ref` names, resolved by
    /// `resolver` at the moment of insertion. When `expect_origin` is given,
    /// the fill is refused unless the field's owning frame has that origin.
    /// Validation and node-bound DOM insertion are synchronous; emitted
    /// input/change events are not trusted keyboard events.
    pub async fn fill_secret(
        &self,
        target: &str,
        secret_ref: &str,
        resolver: &dyn SecretResolver,
        expect_origin: Option<&str>,
    ) -> Result<()> {
        let mut web = self.web().await?;
        let secret = resolver.resolve(secret_ref).await?;
        self.session.check(&self.principal)?;
        web.fill_secret(target, &secret, expect_origin).await
    }

    /// Focus an element by ref or CSS selector.
    pub async fn focus(&self, target: &str) -> Result<()> {
        self.web().await?.focus(target).await
    }

    /// Press a chord inside the page (it reaches the page's focused element,
    /// not the address bar).
    pub async fn press(&self, chord: &str) -> Result<()> {
        self.web().await?.press(chord).await
    }

    /// Press a chord inside the page, hold it for `duration` (at most
    /// [`MAX_HOLD`]), and release it.
    pub async fn press_and_hold(&self, chord: &str, duration: Duration) -> Result<()> {
        self.web().await?.hold(chord, duration).await
    }

    /// Hold the keys of a chord down inside the page until
    /// [`Agent::key_up`] or [`Agent::release_keys`].
    pub async fn key_down(&self, chord: &str) -> Result<()> {
        self.web().await?.key_down(chord).await
    }

    /// Release held page keys.
    pub async fn key_up(&self, chord: &str) -> Result<()> {
        self.web().await?.key_up(chord).await
    }

    /// Release every held page key.
    pub async fn release_keys(&self) -> Result<()> {
        self.web().await?.release_keys().await
    }

    /// Page keys held down, in press order.
    pub async fn held_keys(&self) -> Result<Vec<String>> {
        Ok(self.web().await?.held_keys())
    }

    /// Evaluate a script expression in the page.
    pub async fn eval(&self, expression: &str) -> Result<Value> {
        self.web().await?.eval(expression).await
    }

    /// Outer HTML of an element, or of the document.
    pub async fn html(&self, target: Option<&str>) -> Result<String> {
        self.web().await?.html(target).await
    }

    /// Cookies sent to `urls` (the active page's URL when empty).
    pub async fn cookies(&self, urls: &[String]) -> Result<Vec<Cookie>> {
        self.web().await?.cookies(urls).await
    }

    /// Set cookies.
    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        self.web().await?.set_cookies(cookies).await
    }

    /// Delete cookies by name for a URL (the active page's when `None`).
    pub async fn delete_cookies(&self, name: &str, url: Option<&str>) -> Result<()> {
        self.web().await?.delete_cookies(name, url).await
    }

    /// Delete every cookie of the profile.
    pub async fn clear_cookies(&self) -> Result<()> {
        self.web().await?.clear_cookies().await
    }

    /// PNG of the page's viewport or of the whole page.
    pub async fn page_screenshot(&self, full_page: bool) -> Result<PageImage> {
        self.web().await?.page_screenshot(full_page).await
    }

    // ---- desktop operations ---------------------------------------------

    /// Screenshot of the whole desktop.
    pub async fn screenshot(&self) -> Result<Frame> {
        self.session.check(&self.principal)?;
        #[cfg(not(target_os = "linux"))]
        return Err(Error::Unsupported(
            "desktop screenshots require Linux".into(),
        ));
        #[cfg(target_os = "linux")]
        {
            let socket = self.session.socket.clone();
            let frame = blocking(move || wayland::screenshot(&socket)).await?;
            self.session.check(&self.principal)?;
            Ok(frame)
        }
    }

    /// Run pointer/keyboard steps on the desktop. Keys held by
    /// [`Input::KeyDown`] are released when the steps end.
    pub async fn input(&self, steps: Vec<Input>) -> Result<()> {
        let _web = self.web().await?;
        #[cfg(not(target_os = "linux"))]
        {
            let _ = steps;
            Err(Error::Unsupported("desktop input requires Linux".into()))
        }
        #[cfg(target_os = "linux")]
        {
            let input = self.session.input.clone();
            let last = steps.iter().rev().find_map(|s| match s {
                Input::Move { x, y } => Some((*x, *y)),
                _ => None,
            });
            let result =
                blocking(move || input.lock().map_err(|_| Error::Closed)?.run(&steps)).await;
            if result.is_ok()
                && let Some((x, y)) = last
            {
                self.session.gate.note_pointer(x, y);
            }
            result
        }
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

    /// Press a key chord such as `ctrl+l` on the desktop keyboard.
    ///
    /// Completion acknowledges compositor processing, not the application's
    /// handling of the shortcut. Observe its effect before sending input that
    /// depends on a focus change.
    pub async fn key(&self, chord: &str) -> Result<()> {
        self.input(vec![Input::Key(chord.to_string())]).await
    }

    /// Hold a chord on the desktop keyboard for `duration` (at most
    /// [`MAX_HOLD`]), then release it.
    pub async fn hold_key(&self, chord: &str, duration: Duration) -> Result<()> {
        if duration > MAX_HOLD {
            return Err(Error::InvalidInput(format!(
                "a key is held for at most {} s",
                MAX_HOLD.as_secs()
            )));
        }
        self.input(vec![
            Input::KeyDown(chord.to_string()),
            Input::Wait(duration),
            Input::KeyUp(chord.to_string()),
        ])
        .await
    }

    /// Type text into whatever has keyboard focus.
    ///
    /// The caller must establish the intended application's focus first;
    /// completing a preceding [`Agent::key`] does not acknowledge a UI change.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        self.input(vec![Input::Text(text.to_string())]).await
    }

    /// Scroll at desktop coordinates by notches.
    pub async fn scroll(&self, x: f64, y: f64, dx: i32, dy: i32) -> Result<()> {
        self.input(vec![Input::Move { x, y }, Input::Scroll { dx, dy }])
            .await
    }

    /// Where the desktop pointer is, as last placed by this agent or by a
    /// person during takeover.
    pub fn pointer_position(&self) -> Result<Option<(f64, f64)>> {
        self.session.check(&self.principal)?;
        Ok(self.session.gate.pointer())
    }

    /// The desktop clipboard's text (`None` when it holds no text).
    pub async fn clipboard_text(&self) -> Result<Option<String>> {
        self.session.check(&self.principal)?;
        #[cfg(not(target_os = "linux"))]
        return Err(Error::Unsupported(
            "the desktop clipboard requires Linux".into(),
        ));
        #[cfg(target_os = "linux")]
        {
            let socket = self.session.socket.clone();
            let text = blocking(move || wayland::clipboard_text(&socket)).await?;
            self.session.check(&self.principal)?;
            Ok(text)
        }
    }

    /// Put text on the desktop clipboard (paste it with `ctrl+v`).
    pub async fn set_clipboard_text(&self, text: &str) -> Result<()> {
        self.session.check(&self.principal)?;
        #[cfg(not(target_os = "linux"))]
        {
            let _ = text;
            Err(Error::Unsupported(
                "the desktop clipboard requires Linux".into(),
            ))
        }
        #[cfg(target_os = "linux")]
        {
            let (socket, text) = (self.session.socket.clone(), text.to_string());
            blocking(move || wayland::set_clipboard_text(&socket, text)).await
        }
    }
}

#[cfg(target_os = "linux")]
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Process(format!("desktop worker: {e}")))?
}
