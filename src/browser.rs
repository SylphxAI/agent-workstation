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
    /// Page URL.
    pub url: String,
    /// Indented outline; interactive elements carry `[@eN]`.
    pub text: String,
    /// Number of refs issued.
    pub refs: usize,
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
];

#[derive(Clone, Debug)]
struct NodeRef {
    backend: i64,
    frame: String,
}

/// Web operations over one DevTools connection.
#[derive(Debug)]
pub struct Web {
    cdp: Cdp,
    active: Option<String>,
    refs: HashMap<String, NodeRef>,
}

impl Web {
    /// Connect to the browser endpoint and attach to its first tab.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let mut web = Self {
            cdp: Cdp::connect(endpoint).await?,
            active: None,
            refs: HashMap::new(),
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

    /// Make `id` the active tab (brought to front on the desktop too).
    pub async fn select_tab(&mut self, id: &str) -> Result<()> {
        self.browser_call("Target.activateTarget", json!({ "targetId": id }))
            .await?;
        self.cdp.attach(id).await?;
        self.active = Some(id.to_string());
        self.refs.clear();
        for domain in ["Page.enable", "DOM.enable", "Accessibility.enable"] {
            self.cdp.call(domain, json!({})).await?;
        }
        Ok(())
    }

    /// Close tab `id`; the first remaining tab becomes active.
    pub async fn close_tab(&mut self, id: &str) -> Result<()> {
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
                .find(|target| target != id);
            match next {
                Some(n) => self.select_tab(&n).await?,
                None => {
                    self.new_tab("about:blank").await?;
                }
            }
        }
        Ok(())
    }

    /// Navigate the active tab and wait for the load.
    pub async fn navigate(&mut self, url: &str) -> Result<String> {
        check_url(url)?;
        self.refs.clear();
        let r = self
            .cdp
            .call("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(Error::Protocol(format!("navigation failed: {err}")));
        }
        self.wait_loaded().await?;
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
        self.refs.clear();
        self.cdp
            .call(
                "Page.navigateToHistoryEntry",
                json!({ "entryId": entry["id"] }),
            )
            .await?;
        self.wait_loaded().await?;
        self.current_url().await
    }

    /// Reload the active tab.
    pub async fn reload(&mut self) -> Result<String> {
        self.refs.clear();
        self.cdp.call("Page.reload", json!({})).await?;
        self.wait_loaded().await?;
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

    /// Accessibility snapshot; refs stay valid until the next snapshot or navigation.
    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        self.refs.clear();
        let tree = self.cdp.call("Page.getFrameTree", json!({})).await?;
        let mut frames = vec![&tree["frameTree"]];
        let mut text = Vec::new();
        let mut node_refs = HashMap::new();
        while let Some(frame) = frames.pop() {
            let id = frame["frame"]["id"]
                .as_str()
                .ok_or_else(|| Error::Protocol("frame missing id".into()))?;
            let ax = self
                .cdp
                .call("Accessibility.getFullAXTree", json!({"frameId": id}))
                .await?;
            let nodes = ax["nodes"].as_array().cloned().unwrap_or_default();
            let (outline, refs) = render_tree(&nodes, node_refs.len());
            text.push(outline);
            node_refs.extend(refs.into_iter().map(|(key, backend)| {
                (
                    key,
                    NodeRef {
                        backend,
                        frame: id.to_owned(),
                    },
                )
            }));
            // Local child frames only. Out-of-process frame discovery and
            // session routing are not yet supported by snapshots.
            if let Some(children) = frame["childFrames"].as_array() {
                frames.extend(children.iter().rev());
            }
        }
        let url = self.current_url().await?;
        self.refs = node_refs;
        Ok(Snapshot {
            url,
            refs: self.refs.len(),
            text: text.join("\n"),
        })
    }

    /// Click the element `reference` names.
    pub async fn click(&mut self, reference: &str) -> Result<()> {
        let (x, y) = self.center(reference).await?;
        for (kind, count) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)] {
            self.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": count }),
                )
                .await?;
        }
        Ok(())
    }

    /// Replace the value of the field `reference` names with `text`.
    pub async fn fill(&mut self, reference: &str, text: &str) -> Result<()> {
        self.focus_and_clear(reference).await?;
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
        reference: &str,
        secret: &crate::Secret,
        expect_origin: Option<&str>,
    ) -> Result<()> {
        let node = self
            .refs
            .get(&normalize_ref(reference))
            .cloned()
            .ok_or_else(|| Error::NotFound("secret field is not in the latest snapshot".into()))?;
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

    async fn focus_and_clear(&mut self, reference: &str) -> Result<()> {
        let node = self.node(reference)?;
        self.cdp
            .call("DOM.focus", json!({ "backendNodeId": node }))
            .await?;
        for (kind, key, code, vk, mods) in [
            ("keyDown", "a", "KeyA", 65, 2),
            ("keyUp", "a", "KeyA", 65, 2),
            ("keyDown", "Delete", "Delete", 46, 0),
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

    fn node(&self, reference: &str) -> Result<i64> {
        let key = normalize_ref(reference);
        self.refs
            .get(&key)
            .map(|node| node.backend)
            .ok_or_else(|| Error::NotFound(format!("{reference} is not in the latest snapshot")))
    }

    async fn center(&mut self, reference: &str) -> Result<(f64, f64)> {
        let node = self.node(reference)?;
        let _ = self
            .cdp
            .call(
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": node }),
            )
            .await;
        let q = self
            .cdp
            .call("DOM.getContentQuads", json!({ "backendNodeId": node }))
            .await?;
        let quad = q["quads"][0]
            .as_array()
            .filter(|a| a.len() == 8)
            .ok_or_else(|| Error::NotFound(format!("{reference} is not visible")))?;
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

fn check_url(url: &str) -> Result<()> {
    let u = url::Url::parse(url).map_err(|e| Error::InvalidInput(format!("URL {url:?}: {e}")))?;
    match u.scheme() {
        "http" | "https" | "about" | "data" | "file" => Ok(()),
        s => Err(Error::InvalidInput(format!(
            "URL scheme {s:?} is not navigable"
        ))),
    }
}

/// `@e3`, `e3` → `e3`.
fn normalize_ref(r: &str) -> String {
    r.trim().trim_start_matches('@').to_string()
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
        assert_eq!(normalize_ref("@e2"), "e2");
    }

    #[test]
    fn only_navigable_schemes_are_accepted() {
        assert!(check_url("https://example.com/").is_ok());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("chrome://settings").is_err());
        assert!(check_url("nope").is_err());
    }
}
