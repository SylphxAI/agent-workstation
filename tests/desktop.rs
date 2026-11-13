//! Headful integration on a real headless-Wayland desktop.
//!
//! Runs only with `AGENT_WORKSTATION_HEADFUL=1`. Needs `sway`, `wayvnc` and a
//! Chromium-family browser (`AGENT_WORKSTATION_BROWSER`, default
//! `/usr/bin/chromium`); `AGENT_WORKSTATION_BROWSER_ARGS` adds browser flags.

#![cfg(target_os = "linux")]

use std::time::Duration;

use agent_workstation::cdp::Cdp;
use agent_workstation::{
    BrowserConfig, Button, DesktopSession, Error, Profile, Secret, SecretResolver, SessionConfig,
    TakeoverConfig,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PAGE: &str = r#"<!doctype html><html><head><title>Fixture</title></head><body>
<h1>Sign in</h1>
<label>Email <input id="email" aria-label="Email"></label>
<label>Password <input id="pw" type="password" aria-label="Password"></label>
<button id="go" onclick="document.getElementById('out').textContent='clicked:'+document.getElementById('email').value+':'+document.getElementById('pw').value.length">Continue</button>
<p id="out">idle</p>
<p id="cookie"></p>
<script>
document.getElementById('cookie').textContent = 'cookie=' + document.cookie;
document.cookie = 'kept=yes; max-age=86400; path=/';
// Fault injection: Chromium must wait for the renderer's shortcut handling
// before moving focus to its omnibox. This is not an input-readiness sleep.
addEventListener('keydown', event => {
  if (event.ctrlKey && event.key === 'l') {
    sessionStorage.setItem('delayedShortcutCount', Number(sessionStorage.getItem('delayedShortcutCount') || 0) + 1);
    const until = performance.now() + 500;
    while (performance.now() < until) {} // deliberately stall shortcut ACK
  }
});
</script>
</body></html>"#;

// The fixture owns the only browser window and tab. Observe a page-to-chrome
// focus transition; CDP is used only for observation, never to inject input or
// focus the omnibox. Blur alone is not a general-purpose omnibox detector.
async fn focus_observer(s: &DesktopSession, profile: &std::path::Path) -> Cdp {
    let port = std::fs::read_to_string(profile.join("user-data/DevToolsActivePort")).unwrap();
    let mut lines = port.lines();
    let endpoint = format!(
        "ws://127.0.0.1:{}{}",
        lines.next().unwrap(),
        lines.next().unwrap()
    );
    let mut observer = Cdp::connect(&endpoint).await.unwrap();
    let tabs = s.tabs().await.unwrap();
    assert_eq!(tabs.len(), 1, "focus observation requires one tab");
    observer.attach(&tabs[0].id).await.unwrap();
    observer
}

async fn expect_document_focus(observer: &mut Cdp, expected: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = observer
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": "document.hasFocus()", "returnByValue": true
                    }),
                )
                .await
                .unwrap();
            if result["result"]["value"] == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native input must produce the expected document focus transition");
}

const SECRET: &str = "s3cret-Value-42";

#[derive(Debug)]
struct FixedResolver;

#[async_trait::async_trait]
impl SecretResolver for FixedResolver {
    async fn resolve(&self, secret_ref: &str) -> agent_workstation::Result<Secret> {
        assert_eq!(secret_ref, "vault/item/password");
        Ok(Secret::new(SECRET.into()))
    }
}

async fn serve_fixture() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

fn config(
    profile_dir: &std::path::Path,
    runtime: &std::path::Path,
    takeover: bool,
) -> SessionConfig {
    let mut browser = BrowserConfig::new(
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into()),
    );
    if let Ok(extra) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        browser.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    let mut cfg = SessionConfig::new(Profile::new("fixture", profile_dir), browser, runtime);
    cfg.fingerprint_defaults.screen_width = 1280;
    cfg.fingerprint_defaults.screen_height = 800;
    cfg.takeover = takeover.then(TakeoverConfig::default);
    cfg
}

// Observe navigation completion, never repeat native input or hide its loss.
async fn expect_url(s: &DesktopSession, expected: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let actual = s
            .current_url()
            .await
            .unwrap_or_else(|e| panic!("observing {expected}: {e}"));
        if actual == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "native URL-bar input: expected {expected}, got {actual}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_desktop_web_and_desktop_ops_share_state_and_profile_persists() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }
    let url = serve_fixture().await;
    let profile = tempfile::tempdir().unwrap();
    // An existing profile's fingerprint file, in the established format.
    std::fs::write(
        profile.path().join("fingerprint.json"),
        r#"{"profile":"fixture","seed":42,"user_agent":"","chrome_major":"","chrome_full_version":"","platform":"","oscpu":"","hardware_concurrency":8,"device_memory":8,"screen_width":1280,"screen_height":800,"webgl_vendor":"","webgl_renderer":"","languages":["en-US","en"],"timezone":"UTC"}"#,
    )
    .unwrap();
    let fp_before = std::fs::read(profile.path().join("fingerprint.json")).unwrap();
    let rt = tempfile::tempdir().unwrap();

    let s = DesktopSession::start(config(profile.path(), &rt.path().join("a"), false))
        .await
        .expect("session starts");
    assert_eq!(s.size(), (1280, 800));
    assert_eq!(s.fingerprint().seed, 42);

    // Exercise the native keyboard immediately after startup, before any CDP
    // observation. Shortcut delivery does not acknowledge application handling.
    s.key("ctrl+l").await.unwrap();
    let mut observer = focus_observer(&s, profile.path()).await;
    expect_document_focus(&mut observer, false).await;
    s.type_text(&format!("{url}?startup=1\n")).await.unwrap();
    expect_url(&s, &format!("{url}?startup=1")).await;

    // Web operations.
    assert_eq!(s.navigate(&url).await.unwrap(), url);
    let snap = s.snapshot().await.unwrap();
    assert!(snap.text.contains("heading \"Sign in\""), "{}", snap.text);
    let line_ref = |name: &str| -> String {
        let line = snap
            .text
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} in {}", snap.text));
        line.rsplit("[@")
            .next()
            .unwrap()
            .trim_end_matches(']')
            .to_string()
    };
    let (email, pw, go) = (
        line_ref("textbox \"Email\""),
        line_ref("textbox \"Password\""),
        line_ref("button \"Continue\""),
    );
    s.fill(&email, "person@example.com").await.unwrap();
    s.fill_secret(
        &pw,
        "vault/item/password",
        &FixedResolver,
        Some(url.trim_end_matches('/')),
    )
    .await
    .unwrap();
    let refused = s
        .fill_secret(
            &pw,
            "vault/item/password",
            &FixedResolver,
            Some("https://example.net"),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "refused");
    s.click(&go).await.unwrap();
    let text = s.read().await.unwrap();
    assert!(
        text.contains(&format!("clicked:person@example.com:{}", SECRET.len())),
        "{text}"
    );
    assert!(!text.contains(SECRET) && !s.snapshot().await.unwrap().text.contains(SECRET));

    // Desktop operations on the same window: use the real browser omnibox
    // and virtual keyboard, including a deliberately slow renderer shortcut.
    let frame = s.screenshot().await.unwrap();
    assert_eq!((frame.width, frame.height), (1280, 800));
    assert!(
        frame
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[..3] != frame.rgba[..3]),
        "desktop is not blank"
    );
    s.click_at(640.0, 400.0, Button::Left).await.unwrap();
    expect_document_focus(&mut observer, true).await;
    // A focused document must not satisfy the chrome-focus observation. No
    // shortcut or typing is sent while this bounded negative check runs.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            expect_document_focus(&mut observer, false)
        )
        .await
        .is_err(),
        "page focus must not be mistaken for shortcut completion"
    );
    s.key("ctrl+l").await.unwrap();
    expect_document_focus(&mut observer, false).await;
    s.type_text(&format!("{url}?typed=1\n")).await.unwrap();
    expect_url(&s, &format!("{url}?typed=1")).await;
    let delayed = observer
        .call(
            "Runtime.evaluate",
            json!({
                "expression": "Number(sessionStorage.getItem('delayedShortcutCount'))", "returnByValue": true
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        delayed["result"]["value"], 1,
        "slow shortcut path was exercised"
    );

    if let Ok(dir) = std::env::var("AGENT_WORKSTATION_DEBUG_DIR") {
        std::fs::write(
            format!("{dir}/after-typing.png"),
            s.screenshot().await.unwrap().to_png().unwrap(),
        )
        .unwrap();
    }
    assert!(
        s.current_url().await.unwrap().ends_with("?typed=1"),
        "URL bar typing reached the browser chrome"
    );
    s.click_at(640.0, 400.0, Button::Left).await.unwrap();

    // Human control suspends every agent call and resumes on release.
    s.gate().take(Some("tester".into()));
    for err in [
        s.navigate(&url).await.unwrap_err(),
        s.screenshot().await.unwrap_err(),
        s.key("a").await.unwrap_err(),
    ] {
        assert!(matches!(err, Error::HumanInControl { .. }), "{err}");
    }
    assert!(s.gate().release(Some("done".into())));
    s.navigate(&url).await.unwrap();

    // Tabs.
    for _ in 0..20 {
        let id = s.new_tab(&url).await.unwrap();
        assert_eq!(s.tabs().await.unwrap().len(), 2);
        s.close_tab(&id).await.unwrap();
        let tabs = s.tabs().await.unwrap();
        assert_eq!(tabs.len(), 1);
        assert!(tabs[0].active);
        assert_ne!(tabs[0].id, id);
    }
    s.shutdown().await;

    // Same profile, new session: the cookie set before survives; the
    // fingerprint file is untouched.
    let s = DesktopSession::start(config(profile.path(), &rt.path().join("b"), true))
        .await
        .expect("second session");
    s.navigate(&url).await.unwrap();
    assert!(s.read().await.unwrap().contains("cookie=kept=yes"));
    // The takeover page is served and guarded by its token.
    let page = s.takeover_url().expect("takeover enabled");
    let addr = page.socket_addrs(|| None).unwrap()[0];
    let get = |path: String| async move {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        tcp.read_to_string(&mut out).await.unwrap();
        out
    };
    let token = s.takeover_url().unwrap().query().unwrap().to_string();
    assert!(get(format!("/?{token}")).await.starts_with("HTTP/1.1 200"));
    assert!(get("/".into()).await.starts_with("HTTP/1.1 401"));
    let takeover = s.takeover_url().expect("takeover enabled");
    assert!(takeover.query().unwrap_or_default().starts_with("token="));
    assert_eq!(
        std::fs::read(profile.path().join("fingerprint.json")).unwrap(),
        fp_before
    );
    s.shutdown().await;
}

// These fixtures use same-site, different-port frames, exercising different
// security origins without requiring out-of-process frame session routing.
async fn serve_secret_fields(cross: Option<&str>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fields = |label: &str| {
        format!(
            r#"<!doctype html><input id="chosen" aria-label="{label}" value="before"><input id="other" aria-label="Other" value="untouched"><script>
chosen.addEventListener('focus', () => other.focus());
window.events = [];
const getValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').get;
window.detachedUnchanged = () => getValue.call(oldField) === 'before' && getValue.call(other) === 'untouched';
chosen.addEventListener('input', e => events.push([e.type, e.isTrusted]));
chosen.addEventListener('change', e => events.push([e.type, e.isTrusted]));
// Poison page-world setters: the isolated world must not invoke these.
Object.defineProperty(HTMLInputElement.prototype, 'value', {{set() {{ throw Error('page setter'); }}, get() {{return 'poisoned';}}}});
</script>"#
        )
    };
    let child = fields(if cross.is_some() {
        "Same field"
    } else {
        "Cross field"
    });
    let page = match cross {
        Some(url) => format!(
            "{}<iframe id=same src=/child></iframe><iframe id=cross src={url}></iframe>",
            fields("Top field")
        ),
        None => child.clone(),
    };
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let page = page.clone();
            let child = child.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..n].starts_with(b"GET /child ") {
                    child
                } else {
                    page
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

async fn eval_in(observer: &mut Cdp, expression: &str, context: Option<i64>) -> serde_json::Value {
    let mut args = json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
    if let Some(id) = context {
        args["contextId"] = id.into();
    }
    let result = observer.call("Runtime.evaluate", args).await.unwrap();
    assert!(
        result.get("exceptionDetails").is_none(),
        "fixture evaluation failed"
    );
    result["result"]["value"].clone()
}

// Observe values only as booleans, in a pristine world. Never put a field's
// secret value in assertion output, snapshots, protocol errors or test logs.
async fn field_state(observer: &mut Cdp, frame: &str, filled: bool) {
    let world = observer
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId": frame, "worldName": "fixture-observer"}),
        )
        .await
        .unwrap();
    let context = world["executionContextId"].as_i64().unwrap();
    let expression = format!(
        "document.querySelector('#chosen').value === {} && document.querySelector('#other').value === 'untouched'",
        if filled {
            serde_json::to_string(SECRET).unwrap()
        } else {
            "'before'".into()
        }
    );
    assert_eq!(
        eval_in(observer, &expression, Some(context)).await,
        true,
        "only the selected field may change"
    );
}

struct MutatingResolver<'a> {
    observer: &'a tokio::sync::Mutex<Cdp>,
    expression: &'a str,
}
impl std::fmt::Debug for MutatingResolver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutatingResolver")
    }
}
#[async_trait::async_trait]
impl SecretResolver for MutatingResolver<'_> {
    async fn resolve(&self, _: &str) -> agent_workstation::Result<Secret> {
        eval_in(&mut *self.observer.lock().await, self.expression, None).await;
        Ok(Secret::new(SECRET.into()))
    }
}

fn secret_ref(snapshot: &agent_workstation::Snapshot, name: &str) -> String {
    snapshot
        .text
        .lines()
        .find(|line| line.contains(name))
        .unwrap()
        .split("[@")
        .nth(1)
        .unwrap()
        .trim_end_matches(']')
        .to_owned()
}

fn assert_secret_refusal(error: Error) {
    assert!(matches!(error, Error::Refused(_) | Error::NotFound(_)));
    assert!(!error.to_string().contains(SECRET));
    assert!(!format!("{error:?}").contains(SECRET));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_fill_is_node_and_owning_origin_bound() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }
    let cross = serve_secret_fields(None).await;
    let url = serve_secret_fields(Some(&cross)).await;
    let origin = url.trim_end_matches('/');
    let cross_origin = cross.trim_end_matches('/');
    let profile = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let session = DesktopSession::start(config(profile.path(), runtime.path(), false))
        .await
        .unwrap();
    session.navigate(&url).await.unwrap();
    let mut observer = focus_observer(&session, profile.path()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while eval_in(&mut observer, "document.readyState === 'complete'", None).await != true {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let tree = observer.call("Page.getFrameTree", json!({})).await.unwrap();
    let root = tree["frameTree"]["frame"]["id"].as_str().unwrap();
    let children = tree["frameTree"]["childFrames"].as_array().unwrap();
    let same = children
        .iter()
        .find(|child| child["frame"]["url"].as_str().unwrap().ends_with("/child"))
        .unwrap()["frame"]["id"]
        .as_str()
        .unwrap();
    let different = children
        .iter()
        .find(|child| child["frame"]["url"] == cross)
        .unwrap()["frame"]["id"]
        .as_str()
        .unwrap();
    let snap = session.snapshot().await.unwrap();
    let top_ref = secret_ref(&snap, "Top field");
    let same_ref = secret_ref(&snap, "Same field");
    let cross_ref = secret_ref(&snap, "Cross field");
    // A top-level origin is wrong for a different-origin child. Nothing clears.
    let err = session
        .fill_secret(
            &cross_ref,
            "vault/item/password",
            &FixedResolver,
            Some(origin),
        )
        .await
        .unwrap_err();
    assert_secret_refusal(err);
    for frame in [root, same, different] {
        field_state(&mut observer, frame, false).await;
    }
    // Focus redirection is installed on all fields. A focused-input approach
    // would clear/type into Other; a node-bound operation leaves it unchanged.
    eval_in(&mut observer, "other.focus()", None).await;
    for (reference, expected, frame) in [
        (&top_ref, origin, root),
        (&same_ref, origin, same),
        (&cross_ref, cross_origin, different),
    ] {
        session
            .fill_secret(
                reference,
                "vault/item/password",
                &FixedResolver,
                Some(expected),
            )
            .await
            .unwrap();
        field_state(&mut observer, frame, true).await;
    }
    assert_eq!(eval_in(&mut observer, "events.length === 2 && events.every(e => e[1] === false) && document.activeElement === other", None).await, true);
    // Reset through the pristine world before checking stale-node refusal.
    let world = observer
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId":root,"worldName":"fixture-observer"}),
        )
        .await
        .unwrap();
    let context = world["executionContextId"].as_i64().unwrap();
    eval_in(&mut observer, "chosen.value = 'before'", Some(context)).await;
    let observer = tokio::sync::Mutex::new(observer);
    // Preserve the detached field itself so both old and replacement nodes can
    // be checked after rejection; backend node reuse must never redirect input.
    let resolver = MutatingResolver {
        observer: &observer,
        expression: "window.oldField = chosen; chosen.replaceWith(chosen.cloneNode());",
    };
    assert_secret_refusal(
        session
            .fill_secret(&top_ref, "vault/item/password", &resolver, Some(origin))
            .await
            .unwrap_err(),
    );
    let mut observation = observer.lock().await;
    field_state(&mut observation, root, false).await;
    assert_eq!(
        eval_in(&mut observation, "detachedUnchanged()", None).await,
        true
    );
    // Refresh ref, then replace the entire document while resolving the secret.
    drop(observation);
    let top_ref = secret_ref(&session.snapshot().await.unwrap(), "Top field");
    // A navigation destroys the evaluation context; send it separately and
    // observe the newly loaded document before invoking insertion.
    let resolver = NavigatingResolver {
        observer: &observer,
        url: &format!("{url}?replaced"),
    };
    assert_secret_refusal(
        session
            .fill_secret(&top_ref, "vault/item/password", &resolver, Some(origin))
            .await
            .unwrap_err(),
    );
    field_state(&mut *observer.lock().await, root, false).await;
    session.shutdown().await;
}

struct NavigatingResolver<'a> {
    observer: &'a tokio::sync::Mutex<Cdp>,
    url: &'a str,
}
impl std::fmt::Debug for NavigatingResolver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NavigatingResolver")
    }
}
#[async_trait::async_trait]
impl SecretResolver for NavigatingResolver<'_> {
    async fn resolve(&self, _: &str) -> agent_workstation::Result<Secret> {
        let mut observer = self.observer.lock().await;
        observer
            .call("Page.navigate", json!({"url": self.url}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = observer.call("Runtime.evaluate", json!({"expression": "location.href === ".to_owned() + &serde_json::to_string(self.url).unwrap() + " && document.readyState === 'complete'", "returnByValue":true})).await;
                if let Ok(result) = result && result["result"]["value"] == true { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        Ok(Secret::new(SECRET.into()))
    }
}
