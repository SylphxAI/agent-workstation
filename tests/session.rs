//! Session behaviour on a real headless-Wayland desktop: in-page keys and
//! focus, ref lifetimes, CSS selectors, scripts, cookies, page images, the
//! desktop clipboard and pointer, held keys, mouse history buttons, leases
//! and closing.
//!
//! Runs only with `AGENT_WORKSTATION_HEADFUL=1`; see `tests/desktop.rs` for
//! the environment.

#![cfg(target_os = "linux")]

use std::time::Duration;

use agent_workstation::cdp::Cdp;
use agent_workstation::{
    Agent, BrowserConfig, Button, Cookie, DesktopSession, Error, Input, Profile, SessionConfig,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn headful() -> bool {
    let on = std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
    }
    on
}

const FORM: &str = r#"<!doctype html><title>Form</title>
<form action="/submitted" method="get"><input id="q" name="q" aria-label="Query"></form>
<a href="/next">Next</a>
<button id="mod" style="display:block;width:100%;height:120px" onclick="out.textContent='shift='+event.shiftKey+' ctrl='+event.ctrlKey">Mod</button>
<p id="out">idle</p>
<script>
window.keys = [];
addEventListener('keydown', e => keys.push(['down', e.key, e.repeat, performance.now()]));
addEventListener('keyup', e => keys.push(['up', e.key, false, performance.now()]));
</script>
<div style="height:3000px"></div><p>bottom</p>"#;

/// Serves `FORM` at every path (so navigations stay on the fixture), and
/// sets an HttpOnly cookie.
async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nset-cookie: sid=abc; HttpOnly; Path=/\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{FORM}",
                    FORM.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

fn config(profile: &std::path::Path, runtime: &std::path::Path) -> SessionConfig {
    let mut browser = BrowserConfig::new(
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into()),
    );
    if let Ok(extra) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        browser.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    let mut cfg = SessionConfig::new(Profile::new("fixture", profile), browser, runtime);
    cfg.fingerprint_defaults.screen_width = 1280;
    cfg.fingerprint_defaults.screen_height = 800;
    cfg.takeover = None;
    cfg
}

/// A second DevTools client on the active tab, for observation only.
async fn observer(a: &Agent<'_>, profile: &std::path::Path) -> Cdp {
    let port = std::fs::read_to_string(profile.join("user-data/DevToolsActivePort")).unwrap();
    let mut lines = port.lines();
    let endpoint = format!(
        "ws://127.0.0.1:{}{}",
        lines.next().unwrap(),
        lines.next().unwrap()
    );
    let mut cdp = Cdp::connect(&endpoint).await.unwrap();
    let active = a
        .tabs()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.active)
        .unwrap();
    cdp.attach(&active.id).await.unwrap();
    cdp
}

async fn eval(cdp: &mut Cdp, expression: &str) -> Value {
    cdp.call(
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true }),
    )
    .await
    .unwrap()["result"]["value"]
        .clone()
}

/// Observe a navigation's outcome; never repeat input.
async fn expect_url(a: &Agent<'_>, expected: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let actual = a.current_url().await.unwrap();
        if actual == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected}, got {actual}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn ref_of(snapshot: &agent_workstation::Snapshot, name: &str) -> String {
    let line = snapshot
        .text
        .lines()
        .find(|l| l.contains(name))
        .unwrap_or_else(|| panic!("{name} in {}", snapshot.text));
    format!(
        "@{}",
        line.rsplit("[@").next().unwrap().trim_end_matches(']')
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn page_keys_focus_refs_and_selectors() {
    if !headful() {
        return;
    }
    let url = serve().await;
    let (profile, rt) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let desk = DesktopSession::start(config(profile.path(), &rt.path().join("a")))
        .await
        .unwrap();
    let a = desk.lease("agent-a").unwrap();

    // B6: after a DevTools navigation the page, not the address bar, has
    // keyboard focus, so a desktop Return submits the focused form.
    a.navigate(&url).await.unwrap();
    let mut obs = observer(&a, profile.path()).await;
    assert_eq!(eval(&mut obs, "document.hasFocus()").await, true);
    a.fill("#q", "desktop").await.unwrap();
    a.key("Return").await.unwrap();
    expect_url(&a, &format!("{url}submitted?q=desktop")).await;

    // An in-page Enter reaches the field whatever native focus is on.
    a.navigate(&url).await.unwrap();
    a.key("ctrl+l").await.unwrap();
    let snap = a.snapshot().await.unwrap();
    let query = ref_of(&snap, "textbox \"Query\"");
    a.fill(&query, "page").await.unwrap();
    a.press("Enter").await.unwrap();
    expect_url(&a, &format!("{url}submitted?q=page")).await;
    let mut obs = observer(&a, profile.path()).await;
    // In-page shortcuts never reach the browser: focus stays in the page.
    a.press("ctrl+l").await.unwrap();
    assert_eq!(eval(&mut obs, "document.hasFocus()").await, true);

    // B7: a navigation the page makes invalidates the refs it had.
    let snap = a.snapshot().await.unwrap();
    let next = ref_of(&snap, "link \"Next\"");
    let qualified = format!("@{}:{}", snap.id, next.trim_start_matches('@'));
    a.click(&qualified).await.unwrap();
    expect_url(&a, &format!("{url}next")).await;
    for stale in [next.as_str(), qualified.as_str()] {
        let err = a.click(stale).await.unwrap_err();
        assert!(matches!(err, Error::StaleRef(_)), "{stale}: {err}");
    }
    let snap = a.snapshot().await.unwrap();
    let mut obs = observer(&a, profile.path()).await;
    eval(&mut obs, "location.href = '/scripted'").await;
    expect_url(&a, &format!("{url}scripted")).await;
    let err = a.fill(&ref_of(&snap, "Query"), "x").await.unwrap_err();
    assert!(matches!(err, Error::StaleRef(_)), "{err}");
    // A newer snapshot replaces the older one's refs.
    let old = a.snapshot().await.unwrap();
    let new = a.snapshot().await.unwrap();
    assert_ne!(old.id, new.id);
    let err = a.click(&format!("@{}:e1", old.id)).await.unwrap_err();
    assert!(matches!(err, Error::StaleRef(_)), "{err}");
    a.click(&format!("@{}:e1", new.id)).await.unwrap();
    assert!(matches!(
        a.click("@e999").await.unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(matches!(
        a.click("#absent").await.unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(matches!(
        a.click("@not a ref").await.unwrap_err(),
        Error::InvalidInput(_)
    ));

    // B8: closing the last tab leaves a blank one, and the browser runs on.
    let only = a.tabs().await.unwrap();
    assert_eq!(only.len(), 1);
    a.close_tab(&only[0].id).await.unwrap();
    let tabs = a.tabs().await.unwrap();
    assert_eq!(tabs.len(), 1);
    assert!(tabs[0].active && tabs[0].id != only[0].id);
    a.navigate(&url).await.unwrap();
    assert!(matches!(
        a.close_tab("no-such-tab").await.unwrap_err(),
        Error::NotFound(_)
    ));
    desk.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn page_held_keys_scripts_html_cookies_and_images() {
    if !headful() {
        return;
    }
    let url = serve().await;
    let (profile, rt) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let desk = DesktopSession::start(config(profile.path(), &rt.path().join("a")))
        .await
        .unwrap();
    let a = desk.lease("agent-a").unwrap();
    a.navigate(&url).await.unwrap();

    // Held page modifiers apply to clicks until released.
    a.key_down("shift").await.unwrap();
    assert_eq!(a.held_keys().await.unwrap(), ["Shift"]);
    a.click("#mod").await.unwrap();
    assert_eq!(
        a.eval("out.textContent").await.unwrap(),
        "shift=true ctrl=false"
    );
    a.release_keys().await.unwrap();
    assert!(a.held_keys().await.unwrap().is_empty());
    a.click("#mod").await.unwrap();
    assert_eq!(
        a.eval("out.textContent").await.unwrap(),
        "shift=false ctrl=false"
    );
    a.eval("keys.length = 0").await.unwrap();
    a.press_and_hold("w", Duration::from_millis(300))
        .await
        .unwrap();
    let keys = a.eval("keys").await.unwrap();
    let keys = keys.as_array().unwrap();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_eq!((&keys[0][0], &keys[0][1]), (&json!("down"), &json!("w")));
    assert_eq!((&keys[1][0], &keys[1][1]), (&json!("up"), &json!("w")));
    let held_ms = keys[1][3].as_f64().unwrap() - keys[0][3].as_f64().unwrap();
    assert!(held_ms >= 250.0, "held {held_ms} ms");
    assert!(
        a.press_and_hold("w", Duration::from_secs(11))
            .await
            .is_err()
    );

    // Scripts, HTML.
    assert_eq!(a.eval("1 + 2").await.unwrap(), 3);
    assert_eq!(
        a.eval("Promise.resolve({ a: 1 })").await.unwrap(),
        json!({ "a": 1 })
    );
    assert_eq!(a.eval("undefined").await.unwrap(), Value::Null);
    let err = a.eval("throw new Error('boom')").await.unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("boom")),
        "{err}"
    );
    assert!(
        a.html(Some("#mod"))
            .await
            .unwrap()
            .starts_with("<button id=\"mod\"")
    );
    let snap = a.snapshot().await.unwrap();
    assert!(
        a.html(Some(&ref_of(&snap, "button \"Mod\"")))
            .await
            .unwrap()
            .contains(">Mod</button>")
    );
    assert!(
        a.html(None)
            .await
            .unwrap()
            .starts_with("<!DOCTYPE html><html>")
    );

    // Cookies: HttpOnly cookies are visible to the caller, not the page.
    let cookies = a.cookies(&[]).await.unwrap();
    let sid = cookies.iter().find(|c| c.name == "sid").expect("sid");
    assert_eq!((sid.value.as_str(), sid.http_only), ("abc", Some(true)));
    a.set_cookies(&[Cookie {
        name: "x".into(),
        value: "1".into(),
        url: Some(url.clone()),
        ..Default::default()
    }])
    .await
    .unwrap();
    assert!(
        a.eval("document.cookie")
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .contains("x=1")
    );
    assert!(
        a.set_cookies(&[Cookie {
            name: "y".into(),
            value: "1".into(),
            ..Default::default()
        }])
        .await
        .is_err(),
        "a cookie needs a url or domain"
    );
    a.delete_cookies("x", None).await.unwrap();
    assert!(!a.cookies(&[]).await.unwrap().iter().any(|c| c.name == "x"));
    a.clear_cookies().await.unwrap();
    assert!(a.cookies(&[]).await.unwrap().is_empty());

    // Page images: the viewport, and the whole scrollable page.
    let view = a.page_screenshot(false).await.unwrap();
    let full = a.page_screenshot(true).await.unwrap();
    assert_eq!(&view.png[1..4], b"PNG");
    assert_eq!(&full.png[1..4], b"PNG");
    assert!(view.height < 800 && full.height > 3000, "{view:?} {full:?}");
    assert!(!full.truncated);
    desk.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_clipboard_pointer_held_keys_and_history_buttons() {
    if !headful() {
        return;
    }
    let url = serve().await;
    let (profile, rt) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let desk = DesktopSession::start(config(profile.path(), &rt.path().join("a")))
        .await
        .unwrap();
    let a = desk.lease("agent-a").unwrap();
    assert_eq!(a.pointer_position().unwrap(), Some((640.0, 400.0)));
    a.navigate(&url).await.unwrap();

    // Clipboard, both ways, through the real browser.
    let text = "clip ✓ 42";
    a.set_clipboard_text(text).await.unwrap();
    assert_eq!(a.clipboard_text().await.unwrap().as_deref(), Some(text));
    a.focus("#q").await.unwrap();
    a.key("ctrl+v").await.unwrap();
    assert_eq!(a.eval("q.value").await.unwrap(), text);
    a.eval("q.value = 'copied from the page'").await.unwrap();
    a.focus("#q").await.unwrap();
    a.key("ctrl+a").await.unwrap();
    a.key("ctrl+c").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while a.clipboard_text().await.unwrap().as_deref() != Some("copied from the page") {
        assert!(tokio::time::Instant::now() < deadline, "copy never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A modifier held across pointer input reaches the page's click. The
    // button is tall: the chrome height estimate below is within its margin.
    let center = a
        .eval(
            "(() => { const r = mod.getBoundingClientRect(); \
             return [r.left + r.width / 2, r.top + r.height / 2 + (outerHeight - innerHeight)]; })()",
        )
        .await
        .unwrap();
    let (x, y) = (center[0].as_f64().unwrap(), center[1].as_f64().unwrap());
    a.input(vec![
        Input::KeyDown("ctrl".into()),
        Input::Move { x, y },
        Input::Button {
            button: Button::Left,
            pressed: true,
        },
        Input::Button {
            button: Button::Left,
            pressed: false,
        },
        Input::KeyUp("ctrl".into()),
    ])
    .await
    .unwrap();
    assert_eq!(
        a.eval("out.textContent").await.unwrap(),
        "shift=false ctrl=true"
    );
    assert_eq!(a.pointer_position().unwrap(), Some((x, y)));
    // Keys still held when the steps end are released.
    a.input(vec![Input::KeyDown("shift".into())]).await.unwrap();
    a.click_at(x, y, Button::Left).await.unwrap();
    assert_eq!(
        a.eval("out.textContent").await.unwrap(),
        "shift=false ctrl=false"
    );

    // A held desktop key is one press and one release, the hold apart.
    a.focus("#q").await.unwrap();
    a.eval("keys.length = 0").await.unwrap();
    a.hold_key("a", Duration::from_millis(300)).await.unwrap();
    let keys = a.eval("keys.filter(k => !k[2])").await.unwrap();
    let keys = keys.as_array().unwrap();
    assert_eq!(keys.len(), 2, "{keys:?}");
    let held_ms = keys[1][3].as_f64().unwrap() - keys[0][3].as_f64().unwrap();
    assert!(held_ms >= 250.0, "held {held_ms} ms");

    // History mouse buttons.
    a.navigate(&format!("{url}two")).await.unwrap();
    a.click_at(640.0, 600.0, Button::Back).await.unwrap();
    expect_url(&a, &url).await;
    a.click_at(640.0, 600.0, Button::Forward).await.unwrap();
    expect_url(&a, &format!("{url}two")).await;
    desk.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_agent_holds_a_desktop_and_close_waits_for_calls() {
    if !headful() {
        return;
    }
    let url = serve().await;
    let (profile, rt) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let desk = DesktopSession::start(config(profile.path(), &rt.path().join("a")))
        .await
        .unwrap();
    let a = desk.lease("agent-a").unwrap();
    a.navigate(&url).await.unwrap();
    let err = desk.lease("agent-b").unwrap_err();
    assert!(
        matches!(&err, Error::Leased { principal, .. } if principal == "agent-a"),
        "{err}"
    );
    assert_eq!(desk.leaseholder().unwrap().principal, "agent-a");
    // A person's control pauses the holder.
    desk.gate().take(None);
    assert!(matches!(
        a.read().await.unwrap_err(),
        Error::HumanInControl { .. }
    ));
    desk.gate().release(Some("done".into()));
    assert!(a.read().await.is_ok());

    // Handing the desktop over: the old view is refused from then on.
    let stale_view = a.clone();
    assert!(a.release());
    let b = desk.lease("agent-b").unwrap();
    assert!(matches!(
        stale_view.read().await.unwrap_err(),
        Error::Leased { .. }
    ));
    assert_eq!(b.current_url().await.unwrap(), url);

    // Close waits for a call in flight, then refuses new ones.
    let started = tokio::time::Instant::now();
    let (held, ()) = tokio::join!(
        b.press_and_hold("shift", Duration::from_millis(500)),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            desk.close().await;
        }
    );
    held.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert!(matches!(b.read().await.unwrap_err(), Error::Closed));
    assert!(matches!(desk.lease("agent-c").unwrap_err(), Error::Closed));
    desk.close().await;
}
