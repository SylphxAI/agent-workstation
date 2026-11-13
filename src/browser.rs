//! The session's one browser and its web operations.
//!
//! The browser runs headful on the session's Wayland desktop with a
//! persistent, caller-owned profile directory. Web operations drive that same
//! window over the browser's loopback DevTools endpoint. Ordinary input uses
//! trusted browser events; secret fills use a node-bound isolated-world DOM
//! operation so origin validation and insertion cannot be separated by focus
//! changes or navigation. No fingerprint-spoofing scripts are injected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use crate::cdp::Cdp;
use crate::error::{Error, Result};
use crate::fingerprint::Fingerprint;

mod keys;

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Snapshot text beyond this many lines is cut.
const MAX_SNAPSHOT_LINES: usize = 2_000;

/// How to launch the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserConfig {
    /// Chromium-family binary.
    pub binary: PathBuf,
    /// When set, the fingerprint seed is passed as `<flag>=<seed>` (for
    /// browser builds that derive their fingerprint from a seed).
    pub seed_flag: Option<String>,
    /// Extra arguments appended verbatim.
    pub extra_args: Vec<String>,
}

impl BrowserConfig {
    /// A config for `binary` with no seed flag and no extra arguments.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            seed_flag: None,
            extra_args: Vec::new(),
        }
    }

    /// Command-line arguments (exposed for inspection).
    pub fn args(
        &self,
        user_data_dir: &Path,
        fp: &Fingerprint,
        proxy_url: Option<&str>,
        start_url: &str,
    ) -> Vec<String> {
        let mut a = vec![
            "--ozone-platform=wayland".to_string(),
            format!("--user-data-dir={}", user_data_dir.display()),
            "--remote-debugging-port=0".into(),
            "--remote-debugging-address=127.0.0.1".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--password-store=basic".into(),
            "--disable-blink-features=AutomationControlled".into(),
            "--start-maximized".into(),
            format!("--accept-lang={}", fp.accept_language()),
            format!(
                "--lang={}",
                fp.languages.first().map(String::as_str).unwrap_or("en-US")
            ),
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        ];
        if let Some(flag) = &self.seed_flag {
            a.push(format!("{flag}={}", fp.seed));
        }
        if let Some(p) = proxy_url {
            a.push(format!("--proxy-server={p}"));
        }
        a.extend(self.extra_args.iter().cloned());
        a.push(start_url.to_string());
        a
    }
}

/// The running browser.
#[derive(Debug)]
pub struct Browser {
    child: Child,
    endpoint: String,
}

impl Browser {
    /// Launch on the desktop reached through `desktop_env`.
    pub async fn launch(
        config: &BrowserConfig,
        desktop_env: &[(String, String)],
        user_data_dir: &Path,
        fp: &Fingerprint,
        proxy_url: Option<&str>,
        log: &Path,
    ) -> Result<Self> {
        fp.validate()?;
        std::fs::create_dir_all(user_data_dir)?;
        crate::profile::clear_stale_singleton(user_data_dir)?;
        let port_file = user_data_dir.join("DevToolsActivePort");
        if let Err(e) = std::fs::remove_file(&port_file)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }
        let log = std::fs::File::create(log)?;
        let mut child = Command::new(&config.binary)
            .args(config.args(user_data_dir, fp, proxy_url, "about:blank"))
            .env_remove("DISPLAY")
            .envs(desktop_env.iter().cloned())
            .envs(fp.launch_env())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|e| {
                Error::Process(format!("cannot start {}: {e}", config.binary.display()))
            })?;
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Process(format!(
                    "browser exited during start-up: {status}"
                )));
            }
            if let Some((port, path)) = read_port_file(&port_file)
                && tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
            {
                return Ok(Self {
                    child,
                    endpoint: format!("ws://127.0.0.1:{port}{path}"),
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                return Err(Error::Timeout("browser DevTools endpoint".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn pid(&self) -> Result<u32> {
        self.child
            .id()
            .ok_or_else(|| Error::Process("browser exited during start-up".into()))
    }

    /// Browser-level DevTools WebSocket URL (loopback).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Whether the browser is alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Close gracefully (flushing the profile), then kill after a grace period.
    pub async fn shutdown(mut self) {
        if let Ok(mut cdp) = Cdp::connect(&self.endpoint).await {
            let _ =
                tokio::time::timeout(Duration::from_secs(3), cdp.call("Browser.close", json!({})))
                    .await;
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

fn read_port_file(path: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim().to_string();
    (port != 0 && path.starts_with("/devtools/browser/")).then_some((port, path))
}

/// A tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tab {
    /// Target id.
    pub id: String,
    /// Current URL.
    pub url: String,
    /// Title.
    pub title: String,
    /// Whether web operations act on this tab.
    pub active: bool,
}

/// An accessibility snapshot of the active tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Snapshot id, unique for the life of the browser. Qualified refs
    /// (`@<id>:e3`) name it.
    pub id: String,
    /// Page URL.
    pub url: String,
    /// Indented outline; interactive elements carry `[@eN]`.
    pub text: String,
    /// Number of refs issued.
    pub refs: usize,
}

/// What a web operation acts on.
///
/// Refs come from the latest [`Snapshot`] and are bound to the document each
/// was taken from: any navigation of that document (a link, a form, a
/// script, history, reload) invalidates them, and a newer snapshot replaces
/// them. CSS selectors resolve in the active tab's main document at the
/// moment of the call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Locator {
    /// `@e3` or `e3` (the latest snapshot), or `@<snapshot id>:e3`.
    Ref {
        /// The snapshot the ref was issued by, when qualified.
        snapshot: Option<String>,
        /// `e3`.
        id: String,
    },
    /// A CSS selector.
    Css(String),
}

impl Locator {
    /// `@e3`, `e3` and `@<snapshot>:e3` are refs; anything else is a CSS
    /// selector. A malformed `@` ref is refused rather than read as CSS.
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(Error::InvalidInput("empty element locator".into()));
        }
        let body = s.strip_prefix('@');
        let candidate = body.unwrap_or(s);
        let (snapshot, id) = match candidate.rsplit_once(':') {
            Some((snap, id)) if body.is_some() => (Some(snap), id),
            _ => (None, candidate),
        };
        let is_ref = id.len() > 1
            && id.starts_with('e')
            && id[1..].bytes().all(|b| b.is_ascii_digit())
            && snapshot.is_none_or(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            });
        match (is_ref, body.is_some()) {
            (true, _) => Ok(Locator::Ref {
                snapshot: snapshot.map(String::from),
                id: id.to_string(),
            }),
            (false, true) => Err(Error::InvalidInput(format!(
                "{s:?} is not a ref; refs look like @e3 or @<snapshot id>:e3"
            ))),
            (false, false) => Ok(Locator::Css(s.to_string())),
        }
    }
}

/// A web-page image.
#[derive(Clone, PartialEq, Eq)]
pub struct PageImage {
    /// PNG bytes.
    pub png: Vec<u8>,
    /// Width in CSS pixels.
    pub width: u32,
    /// Height in CSS pixels.
    pub height: u32,
    /// Whether a full-page capture was cut at [`MAX_PAGE_HEIGHT`].
    pub truncated: bool,
}

impl std::fmt::Debug for PageImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PageImage({}x{}{})",
            self.width,
            self.height,
            if self.truncated { ", truncated" } else { "" }
        )
    }
}

/// Tallest full-page capture, in CSS pixels; taller pages are cut.
pub const MAX_PAGE_HEIGHT: u32 = 16_384;

/// A cookie, in DevTools field names. Reads fill every field the browser
/// knows; writes need `name`, `value`, and `url` or `domain`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    /// Name.
    pub name: String,
    /// Value.
    pub value: String,
    /// URL the cookie is set for (writes only): domain, path and secure
    /// default from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Expiry, seconds since the Unix epoch; absent or negative for a
    /// session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    /// `HttpOnly`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    /// `Secure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    /// `Strict`, `Lax` or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

const SECRET_FILL: &str = r#"function(expected, value) {
    if (this.ownerDocument !== document || !this.isConnected ||
        (expected !== null && (globalThis.origin === 'null' || globalThis.origin !== expected)))
        return 'refused';
    if (this.matches(':disabled') || this.readOnly) return 'unsupported';
    let prototype;
    if (this instanceof HTMLInputElement) {
        if (!['text', 'search', 'tel', 'url', 'email', 'password'].includes(this.type))
            return 'unsupported';
        prototype = HTMLInputElement.prototype;
    }
    else if (this instanceof HTMLTextAreaElement) prototype = HTMLTextAreaElement.prototype;
    else return 'unsupported';
    Object.getOwnPropertyDescriptor(prototype, 'value').set.call(this, value);
    this.dispatchEvent(new Event('input', {bubbles: true, composed: true}));
    this.dispatchEvent(new Event('change', {bubbles: true}));
    return 'ok';
}"#;

const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "switch",
    "slider",
    "spinbutton",
    "treeitem",
];
const STRUCTURAL: &[&str] = &[
    "heading",
    "img",
    "dialog",
    "alert",
    "table",
    "row",
    "cell",
    "list",
    "listitem",
    "navigation",
    "main",
    "form",
    // Out-of-process frames show here without their content; reach them
    // through desktop operations.
    "Iframe",
];

/// A DOM node an operation acts on, with the document it belongs to.
#[derive(Clone, Debug)]
struct NodeRef {
    backend: i64,
    frame: String,
}

/// The refs of the latest snapshot and the documents they point into.
#[derive(Debug)]
struct Refs {
    id: String,
    nodes: HashMap<String, NodeRef>,
    /// Loader of every frame the snapshot read, by frame id.
    loaders: HashMap<String, String>,
    main_frame: String,
}

/// The frames of the active tab: frame id → loader id, and the main frame.
struct Frames {
    loaders: HashMap<String, String>,
    main: String,
}

/// Web operations over one DevTools connection.
#[derive(Debug)]
pub struct Web {
    cdp: Cdp,
    active: Option<String>,
    refs: Option<Refs>,
    /// Why the latest snapshot's refs were dropped, for the stale-ref error.
    dropped: Option<String>,
    nonce: String,
    snapshots: u64,
    /// Keys a page holds down (`key_down` without `key_up`), in press order.
    held: Vec<keys::Key>,
}

impl Web {
    /// Connect to the browser endpoint and attach to its first tab.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let mut web = Self {
            cdp: Cdp::connect(endpoint).await?,
            active: None,
            refs: None,
            dropped: None,
            nonce: nonce()?,
            snapshots: 0,
            held: Vec::new(),
        };
        web.browser_call("Target.setDiscoverTargets", json!({ "discover": true }))
            .await?;
        let first = web
            .cdp
            .pages()
            .await?
            .into_iter()
            .find_map(|t| t["targetId"].as_str().map(String::from));
        match first {
            Some(id) => web.select_tab(&id).await?,
            None => {
                web.new_tab("about:blank").await?;
            }
        }
        Ok(web)
    }

    /// Open tabs.
    pub async fn tabs(&mut self) -> Result<Vec<Tab>> {
        Ok(self
            .cdp
            .pages()
            .await?
            .into_iter()
            .map(|t| {
                let id = t["targetId"].as_str().unwrap_or_default().to_string();
                Tab {
                    active: self.active.as_deref() == Some(id.as_str()),
                    url: t["url"].as_str().unwrap_or_default().into(),
                    title: t["title"].as_str().unwrap_or_default().into(),
                    id,
                }
            })
            .collect())
    }

    /// Open a tab at `url` and make it active.
    pub async fn new_tab(&mut self, url: &str) -> Result<String> {
        check_url(url)?;
        let r = self
            .browser_call("Target.createTarget", json!({ "url": url }))
            .await?;
        let id = r["targetId"]
            .as_str()
            .ok_or_else(|| Error::Protocol("createTarget returned no id".into()))?
            .to_string();
        self.select_tab(&id).await?;
        Ok(id)
    }

    /// Make `id` the active tab: brought to front on the desktop, with
    /// keyboard focus in its page.
    pub async fn select_tab(&mut self, id: &str) -> Result<()> {
        // Held keys belong to the page that saw them go down.
        if self.active.is_some() {
            let _ = self.release_keys().await;
        }
        self.held.clear();
        self.browser_call("Target.activateTarget", json!({ "targetId": id }))
            .await?;
        self.cdp.attach(id).await?;
        self.active = Some(id.to_string());
        self.drop_refs("another tab was selected");
        for domain in ["Page.enable", "DOM.enable", "Accessibility.enable"] {
            self.cdp.call(domain, json!({})).await?;
        }
        self.focus_page().await
    }

    /// Close tab `id` and return once the browser has destroyed it. Closing
    /// the active tab activates the first remaining one; closing the last
    /// tab opens a blank one first, so the browser window never closes.
    pub async fn close_tab(&mut self, id: &str) -> Result<()> {
        let pages: Vec<String> = self
            .cdp
            .pages()
            .await?
            .into_iter()
            .filter_map(|t| t["targetId"].as_str().map(String::from))
            .collect();
        if !pages.iter().any(|p| p == id) {
            return Err(Error::NotFound(format!("no tab {id}")));
        }
        if pages.len() == 1 {
            self.new_tab("about:blank").await?;
        }
        let result = self
            .browser_call("Target.closeTarget", json!({ "targetId": id }))
            .await?;
        if result["success"].as_bool() != Some(true) {
            return Err(Error::Protocol("target close was not accepted".into()));
        }
        self.cdp
            .wait_target_destroyed(id, Duration::from_secs(10))
            .await?;
        if self.active.as_deref() == Some(id) {
            self.cdp.detach();
            self.active = None;
            let next = self
                .cdp
                .pages()
                .await?
                .into_iter()
                .filter_map(|t| t["targetId"].as_str().map(String::from))
                .find(|target| target != id)
                .ok_or_else(|| Error::Protocol("no tab remains after close".into()))?;
            self.select_tab(&next).await?;
        }
        Ok(())
    }

    /// Navigate the active tab and wait for the load. Keyboard focus moves
    /// into the page, as after typing an address.
    pub async fn navigate(&mut self, url: &str) -> Result<String> {
        check_url(url)?;
        self.drop_refs("the page navigated");
        let r = self
            .cdp
            .call("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(Error::Protocol(format!("navigation failed: {err}")));
        }
        self.wait_loaded().await?;
        self.focus_page().await?;
        self.current_url().await
    }

    /// History back (`-1`) or forward (`1`).
    pub async fn history(&mut self, delta: i64) -> Result<String> {
        let h = self
            .cdp
            .call("Page.getNavigationHistory", json!({}))
            .await?;
        let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
        let entry = h["entries"]
            .get(idx as usize)
            .filter(|_| idx >= 0)
            .ok_or_else(|| Error::NotFound("no history entry".into()))?;
        self.drop_refs("the page navigated");
        self.cdp
            .call(
                "Page.navigateToHistoryEntry",
                json!({ "entryId": entry["id"] }),
            )
            .await?;
        self.wait_loaded().await?;
        self.focus_page().await?;
        self.current_url().await
    }

    /// Reload the active tab.
    pub async fn reload(&mut self) -> Result<String> {
        self.drop_refs("the page reloaded");
        self.cdp.call("Page.reload", json!({})).await?;
        self.wait_loaded().await?;
        self.focus_page().await?;
        self.current_url().await
    }

    /// URL of the active tab.
    pub async fn current_url(&mut self) -> Result<String> {
        // Browser-owned target metadata survives renderer replacement during
        // native navigation; Page.getNavigationHistory requires an active
        // renderer and can fail between the old page and its replacement.
        let id = self.active.clone().ok_or(Error::Closed)?;
        let result = self
            .browser_call("Target.getTargetInfo", json!({ "targetId": id }))
            .await?;
        result["targetInfo"]["url"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::Protocol("target info missing URL".into()))
    }

    /// Visible text of the active tab (form field values are not included).
    pub async fn read(&mut self) -> Result<String> {
        let r = self
            .cdp
            .call("Runtime.evaluate", json!({ "expression": "document.body ? document.body.innerText : ''", "returnByValue": true }))
            .await?;
        Ok(r["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Accessibility snapshot of the main document and its in-process child
    /// frames. Its refs replace the previous snapshot's and stay valid until
    /// a newer snapshot or a navigation of the document each points into.
    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        self.refs = None;
        let tree = self.cdp.call("Page.getFrameTree", json!({})).await?;
        let mut frames = vec![&tree["frameTree"]];
        let mut text = Vec::new();
        let mut nodes_by_ref = HashMap::new();
        let mut loaders = HashMap::new();
        let main_frame = frame_id(&tree["frameTree"])?;
        while let Some(frame) = frames.pop() {
            let id = frame_id(frame)?;
            loaders.insert(
                id.clone(),
                frame["frame"]["loaderId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            let ax = self
                .cdp
                .call("Accessibility.getFullAXTree", json!({"frameId": id}))
                .await?;
            let nodes = ax["nodes"].as_array().cloned().unwrap_or_default();
            let (outline, refs) = render_tree(&nodes, nodes_by_ref.len());
            text.push(outline);
            nodes_by_ref.extend(refs.into_iter().map(|(key, backend)| {
                (
                    key,
                    NodeRef {
                        backend,
                        frame: id.clone(),
                    },
                )
            }));
            // Local child frames only. Out-of-process frames are listed by
            // their `Iframe` node and reached through desktop operations.
            if let Some(children) = frame["childFrames"].as_array() {
                frames.extend(children.iter().rev());
            }
        }
        let url = self.current_url().await?;
        self.snapshots += 1;
        let id = format!("{}-{}", self.nonce, self.snapshots);
        let refs = nodes_by_ref.len();
        self.refs = Some(Refs {
            id: id.clone(),
            nodes: nodes_by_ref,
            loaders,
            main_frame,
        });
        self.dropped = None;
        Ok(Snapshot {
            id,
            url,
            refs,
            text: text.join("\n"),
        })
    }

    /// Click the element `target` names (a ref or a CSS selector). Keys held
    /// with [`Web::key_down`] apply as modifiers.
    pub async fn click(&mut self, target: &str) -> Result<()> {
        let node = self.resolve(target).await?;
        let (x, y) = self.center(&node, target).await?;
        let modifiers = self.held_modifiers();
        for (kind, count) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)] {
            self.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": count, "modifiers": modifiers }),
                )
                .await?;
        }
        Ok(())
    }

    /// Replace the value of the field `target` names with `text`, typed as
    /// trusted input. The field keeps focus.
    pub async fn fill(&mut self, target: &str, text: &str) -> Result<()> {
        let node = self.resolve(target).await?;
        self.focus_and_clear(&node).await?;
        self.cdp
            .call("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    /// Replace a field's value in its owning frame. Origin validation and
    /// insertion run synchronously on the same node in an isolated world.
    /// Input/change events are dispatched but are not trusted keyboard events.
    /// The secret is never returned or included in protocol error messages.
    pub async fn fill_secret(
        &mut self,
        target: &str,
        secret: &crate::Secret,
        expect_origin: Option<&str>,
    ) -> Result<()> {
        let node = self.resolve(target).await?;
        let refused = || Error::Refused("secret field changed or origin did not match".into());
        let world = self
            .cdp
            .call(
                "Page.createIsolatedWorld",
                json!({
                    "frameId": node.frame, "worldName": "credential-fill"
                }),
            )
            .await
            .map_err(|_| refused())?;
        let context = world["executionContextId"].as_i64().ok_or_else(refused)?;
        let resolved = self
            .cdp
            .call(
                "DOM.resolveNode",
                json!({
                    "backendNodeId": node.backend, "executionContextId": context
                }),
            )
            .await
            .map_err(|_| refused())?;
        let object = resolved["object"]["objectId"]
            .as_str()
            .ok_or_else(refused)?;
        // Never use focused-element input for secrets: a focus handler can
        // redirect it even without navigation. This function does not yield or
        // invoke page code until after its checks and native DOM mutation.
        let result = self
            .cdp
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object,
                    "functionDeclaration": SECRET_FILL,
                    "arguments": [{"value": expect_origin.map(|o| o.trim_end_matches('/'))},
                                  {"value": secret.expose()}],
                    "returnByValue": true,
                    "silent": true
                }),
            )
            .await;
        let _ = self
            .cdp
            .call("Runtime.releaseObject", json!({"objectId": object}))
            .await;
        let result = result.map_err(|_| Error::Protocol("secret insertion failed".into()))?;
        if result.get("exceptionDetails").is_some() {
            return Err(Error::Protocol("secret insertion failed".into()));
        }
        match result["result"]["value"].as_str() {
            Some("ok") => Ok(()),
            Some("unsupported") => Err(Error::Unsupported(
                "secret fill requires an editable text field".into(),
            )),
            _ => Err(refused()),
        }
    }

    /// Focus the element `target` names.
    pub async fn focus(&mut self, target: &str) -> Result<()> {
        let node = self.resolve(target).await?;
        self.focus_page().await?;
        self.cdp
            .call("DOM.focus", json!({ "backendNodeId": node.backend }))
            .await?;
        Ok(())
    }

    /// Press a key chord such as `Enter` or `ctrl+a` inside the page: it
    /// reaches the page's focused element (never the address bar, and never
    /// a browser shortcut). Keyboard focus first moves into the page, so
    /// later desktop keys reach it too.
    pub async fn press(&mut self, chord: &str) -> Result<()> {
        let keys = keys::chord(chord)?;
        self.focus_page().await?;
        let mut modifiers = self.held_modifiers();
        for k in &keys {
            self.key_event(true, k, modifiers).await?;
            modifiers |= k.modifier;
        }
        for k in keys.iter().rev() {
            modifiers &= !k.modifier;
            self.key_event(false, k, modifiers).await?;
        }
        Ok(())
    }

    /// Press `chord` inside the page and hold it for `duration` (at most
    /// [`MAX_HOLD`]) before releasing it.
    pub async fn hold(&mut self, chord: &str, duration: Duration) -> Result<()> {
        if duration > MAX_HOLD {
            return Err(Error::InvalidInput(format!(
                "a key is held for at most {} s",
                MAX_HOLD.as_secs()
            )));
        }
        self.key_down(chord).await?;
        tokio::time::sleep(duration).await;
        self.key_up(chord).await
    }

    /// Press the keys of `chord` inside the page and keep them down until
    /// [`Web::key_up`] or [`Web::release_keys`]. Held modifiers apply to later
    /// keys and clicks.
    pub async fn key_down(&mut self, chord: &str) -> Result<()> {
        let keys = keys::chord(chord)?;
        self.focus_page().await?;
        for k in keys {
            if self.held.iter().any(|h| h.code == k.code && h.key == k.key) {
                continue;
            }
            let modifiers = self.held_modifiers();
            self.key_event(true, &k, modifiers).await?;
            self.held.push(k);
        }
        Ok(())
    }

    /// Release the keys of `chord` that are held.
    pub async fn key_up(&mut self, chord: &str) -> Result<()> {
        for k in keys::chord(chord)?.iter().rev() {
            if let Some(i) = self
                .held
                .iter()
                .position(|h| h.code == k.code && h.key == k.key)
            {
                let held = self.held.remove(i);
                let modifiers = self.held_modifiers();
                self.key_event(false, &held, modifiers).await?;
            }
        }
        Ok(())
    }

    /// Release every held key, last pressed first.
    pub async fn release_keys(&mut self) -> Result<()> {
        while let Some(k) = self.held.pop() {
            let modifiers = self.held_modifiers();
            self.key_event(false, &k, modifiers).await?;
        }
        Ok(())
    }

    /// Keys held with [`Web::key_down`], as `KeyboardEvent.key` names.
    pub fn held_keys(&self) -> Vec<String> {
        self.held.iter().map(|k| k.key.clone()).collect()
    }

    /// Evaluate `expression` in the page (awaiting a promise) and return its
    /// JSON value; `undefined` is `null`. A thrown exception is
    /// [`Error::Script`].
    pub async fn eval(&mut self, expression: &str) -> Result<Value> {
        let r = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"]
                .as_str()
                .or_else(|| ex["text"].as_str())
                .unwrap_or("exception");
            return Err(Error::Script(text.chars().take(2_000).collect()));
        }
        Ok(r["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    /// The outer HTML of the element `target` names, or of the whole main
    /// document when `None`.
    pub async fn html(&mut self, target: Option<&str>) -> Result<String> {
        let params = match target {
            Some(t) => json!({ "backendNodeId": self.resolve(t).await?.backend }),
            None => {
                let doc = self
                    .cdp
                    .call("DOM.getDocument", json!({ "depth": 0 }))
                    .await?;
                json!({ "nodeId": doc["root"]["nodeId"] })
            }
        };
        let r = self.cdp.call("DOM.getOuterHTML", params).await?;
        Ok(r["outerHTML"].as_str().unwrap_or_default().to_string())
    }

    /// Cookies sent to `urls` (the active page's URL when empty).
    pub async fn cookies(&mut self, urls: &[String]) -> Result<Vec<Cookie>> {
        let urls = if urls.is_empty() {
            vec![self.current_url().await?]
        } else {
            urls.to_vec()
        };
        let r = self
            .cdp
            .call("Network.getCookies", json!({ "urls": urls }))
            .await?;
        serde_json::from_value(r["cookies"].clone())
            .map_err(|e| Error::Protocol(format!("cookies: {e}")))
    }

    /// Set cookies; each needs `url` or `domain`.
    pub async fn set_cookies(&mut self, cookies: &[Cookie]) -> Result<()> {
        if let Some(c) = cookies
            .iter()
            .find(|c| c.name.is_empty() || (c.url.is_none() && c.domain.is_none()))
        {
            return Err(Error::InvalidInput(format!(
                "cookie {:?} needs a name and a url or domain",
                c.name
            )));
        }
        self.cdp
            .call("Network.setCookies", json!({ "cookies": cookies }))
            .await
            .map_err(|e| Error::InvalidInput(format!("cookies refused: {e}")))?;
        Ok(())
    }

    /// Delete cookies named `name` that `url` would receive (the active
    /// page's URL when `None`).
    pub async fn delete_cookies(&mut self, name: &str, url: Option<&str>) -> Result<()> {
        let url = match url {
            Some(u) => u.to_string(),
            None => self.current_url().await?,
        };
        self.cdp
            .call("Network.deleteCookies", json!({ "name": name, "url": url }))
            .await?;
        Ok(())
    }

    /// Delete every cookie of the profile.
    pub async fn clear_cookies(&mut self) -> Result<()> {
        self.browser_call("Storage.clearCookies", json!({})).await?;
        Ok(())
    }

    /// A PNG of the page: its viewport, or the whole scrollable page (cut at
    /// [`MAX_PAGE_HEIGHT`]). Browser chrome and native dialogs are not in it;
    /// a desktop screenshot shows those.
    pub async fn page_screenshot(&mut self, full_page: bool) -> Result<PageImage> {
        let metrics = self.cdp.call("Page.getLayoutMetrics", json!({})).await?;
        let size = |v: &Value| {
            (
                v["width"].as_f64().unwrap_or(0.0).ceil() as u32,
                v["height"].as_f64().unwrap_or(0.0).ceil() as u32,
            )
        };
        let (vw, vh) = size(&metrics["cssLayoutViewport"]);
        let (cw, ch) = size(&metrics["cssContentSize"]);
        let (width, height, truncated, params) = if full_page {
            let width = cw.max(vw).max(1);
            let full = ch.max(vh).max(1);
            let height = full.min(MAX_PAGE_HEIGHT);
            (
                width,
                height,
                full > height,
                json!({
                    "format": "png",
                    "captureBeyondViewport": true,
                    "clip": { "x": 0, "y": 0, "width": width, "height": height, "scale": 1 },
                }),
            )
        } else {
            (vw, vh, false, json!({ "format": "png" }))
        };
        let r = self.cdp.call("Page.captureScreenshot", params).await?;
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            r["data"].as_str().unwrap_or_default(),
        )
        .map_err(|e| Error::Protocol(format!("screenshot data: {e}")))?;
        Ok(PageImage {
            png,
            width,
            height,
            truncated,
        })
    }

    /// The node `target` names, in a document that is still current.
    async fn resolve(&mut self, target: &str) -> Result<NodeRef> {
        let frames = self.frames().await?;
        match Locator::parse(target)? {
            Locator::Css(selector) => {
                let doc = self
                    .cdp
                    .call("DOM.getDocument", json!({ "depth": 0 }))
                    .await?;
                let found = self
                    .cdp
                    .call(
                        "DOM.querySelector",
                        json!({ "nodeId": doc["root"]["nodeId"], "selector": selector }),
                    )
                    .await
                    .map_err(|e| Error::InvalidInput(format!("selector {selector:?}: {e}")))?;
                let node_id = found["nodeId"].as_i64().unwrap_or(0);
                if node_id == 0 {
                    return Err(Error::NotFound(format!("no element matches {selector:?}")));
                }
                let described = self
                    .cdp
                    .call("DOM.describeNode", json!({ "nodeId": node_id }))
                    .await?;
                let backend = described["node"]["backendNodeId"]
                    .as_i64()
                    .ok_or_else(|| Error::Protocol("node has no backend id".into()))?;
                Ok(NodeRef {
                    backend,
                    frame: frames.main,
                })
            }
            Locator::Ref { snapshot, id } => {
                let Some(refs) = &self.refs else {
                    return Err(match &self.dropped {
                        Some(why) => Error::StaleRef(format!(
                            "{target}: {why} since the last snapshot; take a new snapshot"
                        )),
                        None => Error::NotFound(format!(
                            "{target}: no snapshot of this page yet; take one first"
                        )),
                    });
                };
                if let Some(s) = snapshot.as_deref()
                    && s != refs.id
                {
                    return Err(Error::StaleRef(format!(
                        "{target} names snapshot {s}, which snapshot {} replaced",
                        refs.id
                    )));
                }
                let navigated = frames.main != refs.main_frame
                    || frames.loaders.get(&frames.main) != refs.loaders.get(&refs.main_frame);
                let node = refs.nodes.get(&id).cloned();
                let frame_navigated = node
                    .as_ref()
                    .is_some_and(|n| frames.loaders.get(&n.frame) != refs.loaders.get(&n.frame));
                if navigated {
                    self.drop_refs("the page navigated");
                    return Err(Error::StaleRef(format!(
                        "{target}: the page navigated since the last snapshot; take a new snapshot"
                    )));
                }
                let node = node.ok_or_else(|| {
                    Error::NotFound(format!("{target} is not in the latest snapshot"))
                })?;
                if frame_navigated {
                    return Err(Error::StaleRef(format!(
                        "{target}: its frame navigated since the last snapshot; take a new snapshot"
                    )));
                }
                Ok(node)
            }
        }
    }

    async fn frames(&mut self) -> Result<Frames> {
        let tree = self.cdp.call("Page.getFrameTree", json!({})).await?;
        let mut loaders = HashMap::new();
        let mut stack = vec![&tree["frameTree"]];
        while let Some(f) = stack.pop() {
            loaders.insert(
                frame_id(f)?,
                f["frame"]["loaderId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            if let Some(children) = f["childFrames"].as_array() {
                stack.extend(children);
            }
        }
        Ok(Frames {
            main: frame_id(&tree["frameTree"])?,
            loaders,
        })
    }

    fn drop_refs(&mut self, why: &str) {
        if self.refs.take().is_some() || self.dropped.is_some() {
            self.dropped = Some(why.to_string());
        }
    }

    /// Hand native keyboard focus from the browser chrome (the address bar
    /// after a DevTools navigation) to the page.
    async fn focus_page(&mut self) -> Result<()> {
        self.cdp.call("Page.bringToFront", json!({})).await?;
        Ok(())
    }

    fn held_modifiers(&self) -> i64 {
        self.held.iter().fold(0, |m, k| m | k.modifier)
    }

    async fn key_event(&mut self, down: bool, k: &keys::Key, modifiers: i64) -> Result<()> {
        let text = if down { k.text_with(modifiers) } else { None };
        let kind = match (down, text.is_some()) {
            (true, true) => "keyDown",
            (true, false) => "rawKeyDown",
            (false, _) => "keyUp",
        };
        let mut params = json!({
            "type": kind,
            "key": k.key_with(modifiers),
            "code": k.code,
            "windowsVirtualKeyCode": k.vk,
            "modifiers": modifiers,
        });
        if let Some(t) = text {
            params["text"] = json!(t);
            params["unmodifiedText"] = json!(t);
        }
        self.cdp.call("Input.dispatchKeyEvent", params).await?;
        Ok(())
    }

    async fn focus_and_clear(&mut self, node: &NodeRef) -> Result<()> {
        self.cdp
            .call("DOM.focus", json!({ "backendNodeId": node.backend }))
            .await?;
        for (kind, key, code, vk, mods) in [
            ("rawKeyDown", "a", "KeyA", 65, keys::CTRL),
            ("keyUp", "a", "KeyA", 65, keys::CTRL),
            ("rawKeyDown", "Delete", "Delete", 46, 0),
            ("keyUp", "Delete", "Delete", 46, 0),
        ] {
            self.cdp
                .call(
                    "Input.dispatchKeyEvent",
                    json!({ "type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": mods }),
                )
                .await?;
        }
        Ok(())
    }

    async fn center(&mut self, node: &NodeRef, target: &str) -> Result<(f64, f64)> {
        let _ = self
            .cdp
            .call(
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": node.backend }),
            )
            .await;
        let q = self
            .cdp
            .call(
                "DOM.getContentQuads",
                json!({ "backendNodeId": node.backend }),
            )
            .await?;
        let quad = q["quads"][0]
            .as_array()
            .filter(|a| a.len() == 8)
            .ok_or_else(|| Error::NotFound(format!("{target} is not visible")))?;
        let v: Vec<f64> = quad.iter().map(|n| n.as_f64().unwrap_or(0.0)).collect();
        Ok((
            (v[0] + v[2] + v[4] + v[6]) / 4.0,
            (v[1] + v[3] + v[5] + v[7]) / 4.0,
        ))
    }

    async fn wait_loaded(&mut self) -> Result<()> {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        loop {
            let r = self
                .cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "document.readyState", "returnByValue": true }),
                )
                .await;
            if let Ok(v) = r
                && v["result"]["value"] == "complete"
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("page load".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn browser_call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.cdp
            .call_in(None, method, params, crate::cdp::CALL_TIMEOUT)
            .await
    }
}

/// Longest key hold.
pub const MAX_HOLD: Duration = Duration::from_secs(10);

fn frame_id(tree: &Value) -> Result<String> {
    tree["frame"]["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Error::Protocol("frame missing id".into()))
}

/// A short random id that tells one browser's snapshots from another's.
fn nonce() -> Result<String> {
    use std::io::Read as _;
    let mut b = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn check_url(url: &str) -> Result<()> {
    let u = url::Url::parse(url).map_err(|e| Error::InvalidInput(format!("URL {url:?}: {e}")))?;
    match u.scheme() {
        "http" | "https" | "about" | "data" | "file" => Ok(()),
        s => Err(Error::InvalidInput(format!(
            "URL scheme {s:?} is not navigable"
        ))),
    }
}

/// Render AX nodes as an outline; interactive nodes get refs to their DOM node.
fn render_tree(nodes: &[Value], start: usize) -> (String, HashMap<String, i64>) {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n["nodeId"].as_str()?, n)))
        .collect();
    let mut out = Vec::new();
    let mut refs = HashMap::new();
    let Some(root) = nodes.first() else {
        return (String::new(), refs);
    };
    let mut stack = vec![(root, 0usize)];
    while let Some((n, depth)) = stack.pop() {
        if out.len() >= MAX_SNAPSHOT_LINES {
            out.push("… (truncated)".to_string());
            break;
        }
        let role = n["role"]["value"].as_str().unwrap_or("");
        let name = n["name"]["value"].as_str().unwrap_or("").trim();
        let ignored = n["ignored"].as_bool().unwrap_or(false);
        let interactive = INTERACTIVE.contains(&role);
        let shown = !ignored
            && (interactive
                || STRUCTURAL.contains(&role)
                || (role == "StaticText" && !name.is_empty()));
        let mut child_depth = depth;
        if shown {
            let mut line = format!("{}- {role}", "  ".repeat(depth));
            if !name.is_empty() {
                let short: String = name.chars().take(160).collect();
                line.push_str(&format!(" {:?}", short));
            }
            if interactive && let Some(dom) = n["backendDOMNodeId"].as_i64() {
                let r = format!("e{}", start + refs.len() + 1);
                line.push_str(&format!(" [@{r}]"));
                refs.insert(r, dom);
            }
            out.push(line);
            child_depth += 1;
        }
        if let Some(children) = n["childIds"].as_array() {
            for c in children.iter().rev() {
                if let Some(child) = c.as_str().and_then(|id| by_id.get(id)) {
                    stack.push((child, child_depth));
                }
            }
        }
    }
    (out.join("\n"), refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::FingerprintDefaults;

    #[test]
    fn launch_args_are_headful_wayland_with_seed_and_proxy() {
        let fp = Fingerprint::generate("p", &FingerprintDefaults::default());
        let mut cfg = BrowserConfig::new("/usr/bin/chromium");
        cfg.seed_flag = Some("--fingerprint".into());
        let a = cfg.args(
            Path::new("/profiles/p/user-data"),
            &fp,
            Some("http://127.0.0.1:9000"),
            "about:blank",
        );
        assert!(a.contains(&"--ozone-platform=wayland".to_string()));
        assert!(a.contains(&format!("--fingerprint={}", fp.seed)));
        assert!(a.contains(&"--proxy-server=http://127.0.0.1:9000".to_string()));
        assert!(a.contains(&"--user-data-dir=/profiles/p/user-data".to_string()));
        let joined = a.join(" ");
        assert!(!joined.contains("headless") && !joined.contains("enable-automation"));
    }

    #[test]
    fn outline_gives_refs_to_interactive_nodes_only() {
        let nodes: Vec<Value> = serde_json::from_value(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Example"}, "childIds": ["2", "3", "4", "5"]},
            {"nodeId": "2", "role": {"value": "heading"}, "name": {"value": "Sign in"}, "childIds": []},
            {"nodeId": "3", "role": {"value": "textbox"}, "name": {"value": "Email"}, "backendDOMNodeId": 41, "childIds": []},
            {"nodeId": "4", "role": {"value": "button"}, "name": {"value": "Continue"}, "backendDOMNodeId": 42, "childIds": []},
            {"nodeId": "5", "role": {"value": "button"}, "name": {"value": "Hidden"}, "ignored": true, "backendDOMNodeId": 43, "childIds": []}
        ]))
        .unwrap();
        let (text, refs) = render_tree(&nodes, 0);
        assert_eq!(
            text,
            "- heading \"Sign in\"\n- textbox \"Email\" [@e1]\n- button \"Continue\" [@e2]"
        );
        assert_eq!(refs.get("e1"), Some(&41));
        assert_eq!(refs.get("e2"), Some(&42));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn only_navigable_schemes_are_accepted() {
        assert!(check_url("https://example.com/").is_ok());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("chrome://settings").is_err());
        assert!(check_url("nope").is_err());
    }
}
