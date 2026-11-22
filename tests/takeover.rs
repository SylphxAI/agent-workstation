//! Takeover against a real headless sway + wayvnc. Runs only with
//! `AGENT_WORKSTATION_HEADFUL=1` and `sway` and `wayvnc` on `PATH`.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use agent_workstation::browser::Browser;
use agent_workstation::cdp::Cdp;
use agent_workstation::compositor::Compositor;
use agent_workstation::gate::ControlGate;
use agent_workstation::takeover::{Takeover, TakeoverConfig};
use agent_workstation::vnc::Vnc;
use agent_workstation::{BrowserConfig, Fingerprint};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

/// Reads exact byte counts across WebSocket message boundaries.
struct Rfb<S> {
    ws: S,
    buf: Vec<u8>,
}

impl<S> Rfb<S>
where
    S: futures::Stream<Item = tokio_tungstenite::tungstenite::Result<Message>>
        + futures::Sink<Message>
        + Unpin,
    <S as futures::Sink<Message>>::Error: std::fmt::Debug,
{
    async fn read(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("RFB read timed out");
            match msg.expect("stream ended").expect("websocket error") {
                Message::Binary(b) => self.buf.extend_from_slice(&b),
                other => panic!("unexpected message {other:?}"),
            }
        }
        self.buf.drain(..n).collect()
    }

    async fn send(&mut self, bytes: &[u8]) {
        self.ws
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .unwrap();
    }

    async fn init(&mut self) -> (u16, u16) {
        assert_eq!(self.read(12).await, b"RFB 003.008\n");
        self.send(b"RFB 003.008\n").await;
        let count = self.read(1).await[0] as usize;
        assert!(
            self.read(count).await.contains(&1),
            "VNC server offers security type None"
        );
        self.send(&[1]).await;
        assert_eq!(self.read(4).await, [0, 0, 0, 0], "SecurityResult OK");
        self.send(&[1]).await; // ClientInit, shared
        let init = self.read(24).await;
        let (width, height) = (
            u16::from_be_bytes([init[0], init[1]]),
            u16::from_be_bytes([init[2], init[3]]),
        );
        let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
        self.read(name_len).await;
        (width, height)
    }

    async fn pointer(&mut self, x: u16, y: u16) {
        let [xh, xl] = x.to_be_bytes();
        let [yh, yl] = y.to_be_bytes();
        self.send(&[5, 0, xh, xl, yh, yl]).await;
    }

    async fn raw_frame(&mut self, width: u16, height: u16) -> Vec<u8> {
        let [wh, wl] = width.to_be_bytes();
        let [hh, hl] = height.to_be_bytes();
        self.send(&[3, 0, 0, 0, 0, 0, wh, wl, hh, hl]).await;
        let header = self.read(4).await;
        assert_eq!(header[0], 0, "FramebufferUpdate");
        assert_eq!(u16::from_be_bytes([header[2], header[3]]), 1);
        assert_eq!(
            self.read(12).await,
            [0, 0, 0, 0, wh, wl, hh, hl, 0, 0, 0, 0],
            "full Raw rectangle"
        );
        self.read(usize::from(width) * usize::from(height) * 4)
            .await
    }
}

async fn status(addr: std::net::SocketAddr, path: &str) -> String {
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    tcp.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).await.unwrap();
    out.lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn takeover_streams_a_headless_sway_desktop() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let compositor = Compositor::start(Path::new("sway"), dir.path(), 1280, 800)
        .await
        .expect("compositor");
    let wayvnc = Vnc::start(Path::new("wayvnc"), &compositor.client_env(), dir.path())
        .await
        .expect("wayvnc");
    let vnc = wayvnc.socket().to_path_buf();

    let gate = ControlGate::new();
    let takeover = Takeover::start(TakeoverConfig::default(), vnc, gate)
        .await
        .unwrap();
    let addr = takeover.local_addr().unwrap();
    let url = takeover.url();
    let token = url
        .query_pairs()
        .find(|(k, _)| k == "token")
        .unwrap()
        .1
        .into_owned();
    assert!(token.len() >= 43, "generated token carries 32 random bytes");

    assert!(status(addr, "/state").await.contains(" 401 "));
    assert!(status(addr, "/ws?token=wrong").await.contains(" 403 "));
    assert!(
        status(addr, &format!("/?token={token}"))
            .await
            .contains(" 200 ")
    );
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .is_err()
    );

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={token}"))
        .await
        .unwrap();
    let mut rfb = Rfb {
        ws,
        buf: Vec::new(),
    };
    let (width, height) = rfb.init().await;
    assert_eq!((width, height), (1280, 800));
    rfb.send(&[2, 0, 0, 1, 0, 0, 0, 0]).await; // SetEncodings: Raw
    let [w0, w1] = width.to_be_bytes();
    let [h0, h1] = height.to_be_bytes();
    rfb.send(&[3, 0, 0, 0, 0, 0, w0, w1, h0, h1]).await;
    // Keyboard input while the agent holds control is dropped, not fatal.
    rfb.send(&[4, 1, 0, 0, 0, 0, 0, 0x61]).await;
    let header = rfb.read(4).await;
    assert_eq!(header[0], 0, "FramebufferUpdate");
    assert!(
        u16::from_be_bytes([header[2], header[3]]) >= 1,
        "at least one rectangle"
    );
    takeover.shutdown().await;
    wayvnc.shutdown().await;
    compositor.shutdown().await;
}

async fn launch_browser(compositor: &Compositor, dir: &Path, args: &[&str]) -> (Browser, Cdp) {
    let mut config = BrowserConfig::new(
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into()),
    );
    if let Ok(extra) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        config.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    config
        .extra_args
        .extend(args.iter().map(|arg| (*arg).into()));
    let browser = Browser::launch(
        &config,
        &compositor.client_env(),
        &dir.join("profile"),
        &Fingerprint::generate("fixture", &Default::default()),
        None,
        &dir.join("browser.log"),
    )
    .await
    .unwrap();
    let mut cdp = Cdp::connect(browser.endpoint()).await.unwrap();
    let pages = cdp.pages().await.unwrap();
    cdp.attach(pages[0]["targetId"].as_str().unwrap())
        .await
        .unwrap();
    (browser, cdp)
}

async fn evaluate(cdp: &mut Cdp, expression: &str) -> Value {
    let response = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expression, "returnByValue": true, "awaitPromise": true
            }),
        )
        .await
        .unwrap();
    assert!(response.get("exceptionDetails").is_none(), "{response}");
    response["result"]["value"].clone()
}

async fn wait_for(cdp: &mut Cdp, expression: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = evaluate(cdp, expression).await;
        if seen == true {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: {expression}; last: {seen}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

const POINTER_PAGE: &str = r#"<!doctype html><style>
html,body { margin: 0; background: rgb(33, 55, 77); }
#text { position: absolute; left: 140px; top: 110px; font: 24px monospace; }
</style><div id="text">Select these words</div><script>
window.hover = [];
window.clicked = [];
addEventListener('mousemove', e => { window.hover = [e.clientX, e.clientY]; });
addEventListener('click', e => { window.clicked = [e.clientX, e.clientY]; });
</script>"#;

#[tokio::test]
async fn takeover_uses_one_local_cursor_and_keeps_pixel_mapping() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: opt in to headful tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let desktop = Compositor::start(Path::new("sway"), &dir.path().join("desktop"), 1280, 800)
        .await
        .unwrap();
    let (browser, mut page) = launch_browser(&desktop, dir.path(), &["--kiosk"]).await;
    let url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(POINTER_PAGE.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    page.call("Page.navigate", json!({"url":url}))
        .await
        .unwrap();
    wait_for(
        &mut page,
        "Array.isArray(window.hover) && innerWidth === 1280 && innerHeight === 800",
    )
    .await;

    let wayvnc = Vnc::start(
        Path::new("wayvnc"),
        &desktop.client_env(),
        &dir.path().join("desktop"),
    )
    .await
    .unwrap();
    let gate = ControlGate::new();
    gate.take(None);
    let takeover = Takeover::start(TakeoverConfig::default(), wayvnc.socket().into(), gate)
        .await
        .unwrap();
    let url = takeover.url();
    let (ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/ws?{}",
        takeover.local_addr().unwrap(),
        url.query().unwrap()
    ))
    .await
    .unwrap();
    let mut rfb = Rfb {
        ws,
        buf: Vec::new(),
    };
    let (width, height) = rfb.init().await;
    assert_eq!((width, height), (1280, 800));
    // Request little-endian 32-bit BGRX and Raw only, so no cursor rectangle can be mistaken for pixels.
    rfb.send(&[
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
    ])
    .await;
    rfb.send(&[2, 0, 0, 1, 0, 0, 0, 0]).await;
    let points = [(360, 360), (640, 430), (930, 560)];
    let mut mismatches = Vec::new();
    for (step, (x, y)) in points.into_iter().enumerate() {
        rfb.pointer(x, y).await;
        wait_for(
            &mut page,
            &format!("JSON.stringify(window.hover) === '[{x},{y}]'"),
        )
        .await;
        let red = 40 + step as u8;
        evaluate(
            &mut page,
            &format!("document.documentElement.style.backgroundColor = 'rgb({red}, 55, 77)'"),
        )
        .await;
        let pixel = |frame: &[u8], x, y| {
            let i = (usize::from(y) * usize::from(width) + usize::from(x)) * 4;
            [frame[i], frame[i + 1], frame[i + 2]]
        };
        let expected = [77, 55, red];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let frame = loop {
            let frame = rfb.raw_frame(width, height).await;
            // A new paint after the observed move rules out sampling a stale frame.
            if pixel(&frame, 100, 650) == expected {
                break frame;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "new paint never reached the framebuffer"
            );
        };
        let different = points
            .into_iter()
            .flat_map(|(px, py)| {
                (py - 48..py + 48).flat_map(move |ry| (px - 48..px + 48).map(move |rx| (rx, ry)))
            })
            .filter(|&(px, py)| pixel(&frame, px, py) != expected)
            .count();
        mismatches.push(different);
    }
    assert_eq!(
        mismatches,
        [0, 0, 0],
        "non-background pixels around current and previous pointer positions"
    );
    println!(
        "Raw framebuffer: {mismatches:?} non-background pixels across 27,648 sampled pixels per update"
    );
    drop(rfb);
    verify_viewer(dir.path(), &url, &mut page).await;
    takeover.shutdown().await;
    wayvnc.shutdown().await;
    browser.shutdown().await;
    desktop.shutdown().await;
}

/// Send real pointer events to the viewer browser. Only RFB drives the remote page.
async fn viewer_mouse(viewer: &mut Cdp, kind: &str, x: f64, y: f64, buttons: u8) {
    let button = if kind == "mouseMoved" { "none" } else { "left" };
    viewer
        .call(
            "Input.dispatchMouseEvent",
            json!({
                "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": 1,
            }),
        )
        .await
        .unwrap();
}

async fn verify_viewer(dir: &Path, url: &url::Url, page: &mut Cdp) {
    let dir = dir.join("viewer");
    let compositor = Compositor::start(Path::new("sway"), &dir, 1440, 1000)
        .await
        .unwrap();
    let (browser, mut viewer) = launch_browser(&compositor, &dir, &[]).await;
    viewer
        .call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": false,
            }),
        )
        .await
        .unwrap();
    viewer
        .call("Page.navigate", json!({"url":url.as_str()}))
        .await
        .unwrap();
    wait_for(&mut viewer, "document.querySelector('#signal')?.dataset.state === 'control' && document.querySelector('#frame canvas')?.width === 1280").await;
    let frame = evaluate(
        &mut viewer,
        "document.querySelector('#frame canvas').getBoundingClientRect().toJSON()",
    )
    .await;
    assert_eq!(
        (frame["width"].as_f64(), frame["height"].as_f64()),
        (Some(1280.0), Some(800.0))
    );
    let left = frame["left"].as_f64().unwrap();
    let top = frame["top"].as_f64().unwrap();
    let desktop_point = |x: i64, y: i64| (left + x as f64, top + y as f64);
    let (x, y) = desktop_point(510, 340);
    viewer_mouse(&mut viewer, "mouseMoved", x, y, 0).await;
    wait_for(page, "JSON.stringify(window.hover) === '[510,340]'").await;
    let cursor = evaluate(&mut viewer, &format!(r#"(() => {{
      const target = document.elementFromPoint({x}, {y});
      return [target.id, getComputedStyle(target).cursor, document.querySelector('#cursor').hidden, document.querySelectorAll('canvas').length];
    }})()"#)).await;
    assert_eq!(cursor[0], "screen");
    let shape = cursor[1].as_str().unwrap();
    assert!(
        shape == "default" || shape.starts_with("url("),
        "native cursor: {shape}"
    );
    assert_eq!(cursor[2], true, "no local trackpad arrow for mouse input");
    assert_eq!(
        cursor[3], 1,
        "only the framebuffer canvas, no follower overlay"
    );
    viewer_mouse(&mut viewer, "mousePressed", x, y, 1).await;
    viewer_mouse(&mut viewer, "mouseReleased", x, y, 0).await;
    wait_for(page, "JSON.stringify(window.clicked) === '[510,340]'").await;

    let (start, y) = desktop_point(141, 123);
    let (end, _) = desktop_point(400, 123);
    let (mid, _) = desktop_point(255, 123);
    viewer_mouse(&mut viewer, "mouseMoved", start, y, 0).await;
    wait_for(page, "JSON.stringify(window.hover) === '[141,123]'").await;
    viewer_mouse(&mut viewer, "mousePressed", start, y, 1).await;
    viewer_mouse(&mut viewer, "mouseMoved", mid, y, 1).await;
    wait_for(page, "getSelection().toString().startsWith('Select')").await;
    viewer_mouse(&mut viewer, "mouseMoved", end, y, 1).await;
    viewer_mouse(&mut viewer, "mouseReleased", end, y, 0).await;
    wait_for(page, "String(getSelection()) === 'Select these words'").await;

    assert_eq!(
        evaluate(
            &mut viewer,
            "fetch('release', { method: 'POST' }).then(r => r.ok)"
        )
        .await,
        true
    );
    wait_for(
        &mut viewer,
        "document.querySelector('#signal').dataset.state === 'view'",
    )
    .await;
    let prior_input = evaluate(
        page,
        "[window.hover, window.clicked, String(getSelection())]",
    )
    .await;
    let (x, y) = desktop_point(510, 340);
    viewer_mouse(&mut viewer, "mouseMoved", x, y, 0).await;
    viewer_mouse(&mut viewer, "mousePressed", x, y, 1).await;
    viewer_mouse(&mut viewer, "mouseReleased", x, y, 0).await;
    assert_eq!(
        evaluate(
            page,
            "[window.hover, window.clicked, String(getSelection())]"
        )
        .await,
        prior_input
    );
    assert_eq!(
        evaluate(
            &mut viewer,
            "getComputedStyle(document.querySelector('#screen')).cursor"
        )
        .await,
        "default"
    );
    println!(
        "Viewer: native cursor, exact hover/click, text drag, and view-only input suppression verified"
    );
    browser.shutdown().await;
    compositor.shutdown().await;
}

async fn focus_desktop(viewer: &mut Cdp) {
    // noVNC replaces its canvas on reconnect (including around a page reload).
    // Locate and focus the attached canvas in the same browser turn.
    wait_for(viewer, "(() => { const canvas = document.querySelector('#frame canvas'); if (!canvas || !document.hasFocus()) return false; canvas.focus(); return document.activeElement === canvas; })()").await;
}

/// Drive the viewer with Chromium's trusted keyboard events, not a synthetic
/// DOM paste event or the clipboard endpoint directly.
async fn viewer_chord(cdp: &mut Cdp, key: char, modifiers: u8) {
    let upper = key.to_ascii_uppercase();
    let (modifier, vk) = match modifiers {
        2 => ("Control", 17),
        4 => ("Meta", 91),
        _ => panic!("unexpected CDP modifier"),
    };
    let letter = key.to_string();
    let code = format!("Key{upper}");
    let mod_code = format!("{modifier}Left");
    for (kind, key, code, vk, mods) in [
        ("rawKeyDown", modifier, mod_code.as_str(), vk, modifiers),
        ("rawKeyDown", &letter, &code, upper as u32, modifiers),
        ("keyUp", &letter, &code, upper as u32, modifiers),
        ("keyUp", modifier, &mod_code, vk, 0),
    ] {
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({"type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": mods}),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn takeover_shortcuts_sync_the_wayland_clipboard() {
    use agent_workstation::{DesktopSession, Profile, SessionConfig};
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("remote-profile");
    let mut config = BrowserConfig::new(
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into()),
    );
    if let Ok(extra) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        config.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    let mut config = SessionConfig::new(
        Profile::new("clipboard-fixture", &profile),
        config,
        dir.path().join("remote"),
    );
    config.fingerprint_defaults.screen_width = 1280;
    config.fingerprint_defaults.screen_height = 800;
    let desktop = DesktopSession::start(config).await.unwrap();
    let port = std::fs::read_to_string(profile.join("user-data/DevToolsActivePort")).unwrap();
    let mut lines = port.lines();
    let mut remote = Cdp::connect(&format!(
        "ws://127.0.0.1:{}{}",
        lines.next().unwrap(),
        lines.next().unwrap()
    ))
    .await
    .unwrap();
    let pages = remote.pages().await.unwrap();
    remote
        .attach(pages[0]["targetId"].as_str().unwrap())
        .await
        .unwrap();
    remote.call("Page.navigate", json!({"url": "data:text/html,<textarea id=box autofocus></textarea><script>window.pastes=[];window.chords=[];addEventListener('paste',e=>pastes.push(e.clipboardData.getData('text/plain')));addEventListener('keydown',e=>{if(e.ctrlKey||e.metaKey)chords.push([e.key,e.ctrlKey,e.metaKey])});</script>"})).await.unwrap();
    wait_for(
        &mut remote,
        "document.activeElement?.id === 'box' && Array.isArray(window.pastes)",
    )
    .await;
    evaluate(
        &mut remote,
        "box.value='stale selection'; box.select(); document.execCommand('copy'); box.value='';",
    )
    .await;

    let viewer_dir = dir.path().join("viewer");
    let compositor = Compositor::start(Path::new("sway"), &viewer_dir, 1280, 800)
        .await
        .unwrap();
    let (browser, mut viewer) = launch_browser(&compositor, &viewer_dir, &[]).await;
    let url = desktop.takeover_url().unwrap();
    viewer
        .call(
            "Browser.grantPermissions",
            json!({
                "origin": url.origin().ascii_serialization(),
                "permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"],
            }),
        )
        .await
        .unwrap();
    viewer
        .call("Page.navigate", json!({"url":url.as_str()}))
        .await
        .unwrap();
    viewer
        .call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled":true}),
        )
        .await
        .unwrap();
    wait_for(&mut viewer, "document.querySelector('#signal')?.dataset.state === 'view' && !!document.querySelector('#frame canvas') && document.hasFocus()").await;
    focus_desktop(&mut viewer).await;
    let unicode = "粵語 密碼 🔐\n验证码 012345";
    let text = json!(unicode);
    evaluate(
        &mut viewer,
        &format!("navigator.clipboard.writeText({text})"),
    )
    .await;
    viewer_chord(&mut viewer, 'v', 2).await;
    assert_eq!(evaluate(&mut remote, "box.value").await, "");
    evaluate(&mut viewer, "document.querySelector('#take').click()").await;
    wait_for(
        &mut viewer,
        "document.querySelector('#signal')?.dataset.state === 'control'",
    )
    .await;
    focus_desktop(&mut viewer).await;
    viewer_chord(&mut viewer, 'v', 2).await;
    wait_for(
        &mut remote,
        &format!("box.value === {text} && JSON.stringify(pastes) === JSON.stringify([{text}])"),
    )
    .await;

    viewer_chord(&mut viewer, 'a', 2).await;
    wait_for(
        &mut remote,
        &format!("box.selectionStart === 0 && box.selectionEnd === {text}.length"),
    )
    .await;
    evaluate(
        &mut viewer,
        "navigator.clipboard.writeText('viewer sentinel')",
    )
    .await;
    viewer_chord(&mut viewer, 'c', 2).await;
    wait_for(
        &mut viewer,
        &format!("navigator.clipboard.readText().then(s=>s==={text})"),
    )
    .await;
    evaluate(
        &mut viewer,
        "navigator.clipboard.writeText('viewer sentinel')",
    )
    .await;
    viewer_chord(&mut viewer, 'x', 2).await;
    wait_for(&mut remote, "box.value === ''").await;
    wait_for(
        &mut viewer,
        &format!("navigator.clipboard.readText().then(s=>s==={text})"),
    )
    .await;

    // A copy with no shortcut at all must sync, as with a remote context menu.
    let menu_text = json!("選單複製 🐈");
    evaluate(
        &mut remote,
        &format!("box.value={menu_text}; box.select(); document.execCommand('copy')"),
    )
    .await;
    wait_for(
        &mut viewer,
        &format!("navigator.clipboard.readText().then(s=>s==={menu_text})"),
    )
    .await;

    // Run the real page’s macOS branch in this Linux viewer, then send trusted
    // Meta chords. The host's native Cmd+V default is covered by the mapping
    // test, not emulated here; the native paste path above uses Linux Ctrl+V.
    viewer.call("Page.enable", json!({})).await.unwrap();
    viewer.call("Emulation.setUserAgentOverride", json!({
        "userAgent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
        "platform": "MacIntel",
    })).await.unwrap();
    viewer.call("Page.reload", json!({})).await.unwrap();
    wait_for(&mut viewer, "document.querySelector('#signal')?.dataset.state === 'control' && document.hasFocus() && !!document.querySelector('#frame canvas') && navigator.platform === 'MacIntel'").await;
    focus_desktop(&mut viewer).await;
    evaluate(
        &mut viewer,
        "navigator.clipboard.writeText('viewer sentinel')",
    )
    .await;
    evaluate(&mut remote, "box.setSelectionRange(0,0); chords.length=0;").await;
    viewer_chord(&mut viewer, 'a', 4).await;
    wait_for(&mut remote, "box.selectionEnd === box.value.length").await;
    viewer_chord(&mut viewer, 'c', 4).await;
    wait_for(
        &mut viewer,
        &format!("navigator.clipboard.readText().then(s=>s==={menu_text})"),
    )
    .await;
    viewer_chord(&mut viewer, 'x', 4).await;
    wait_for(&mut remote, "box.value === ''").await;
    viewer_chord(&mut viewer, 'z', 4).await;
    wait_for(&mut remote, &format!("box.value === {menu_text}")).await;
    assert_eq!(
        evaluate(
            &mut remote,
            "chords.filter(([key]) => ['a','c','x','z'].includes(key))"
        )
        .await,
        json!([
            ["a", true, false],
            ["c", true, false],
            ["x", true, false],
            ["z", true, false]
        ])
    );

    // Browser rejection offers a clickable retry with the current remote value.
    evaluate(&mut viewer, "window.writeText = navigator.clipboard.writeText.bind(navigator.clipboard); navigator.clipboard.writeText = () => Promise.reject(new Error('blocked'));").await;
    evaluate(
        &mut remote,
        &format!("box.value={text}; box.select(); document.execCommand('copy')"),
    )
    .await;
    wait_for(&mut viewer, "!document.querySelector('#copy-retry').hidden").await;
    let retry = evaluate(
        &mut viewer,
        "document.querySelector('#copy-retry').getBoundingClientRect().toJSON()",
    )
    .await;
    evaluate(
        &mut viewer,
        "navigator.clipboard.writeText = window.writeText",
    )
    .await;
    let x = retry["left"].as_f64().unwrap() + retry["width"].as_f64().unwrap() / 2.0;
    let y = retry["top"].as_f64().unwrap() + retry["height"].as_f64().unwrap() / 2.0;
    viewer_mouse(&mut viewer, "mousePressed", x, y, 1).await;
    viewer_mouse(&mut viewer, "mouseReleased", x, y, 0).await;
    wait_for(
        &mut viewer,
        &format!("navigator.clipboard.readText().then(s=>s==={text})"),
    )
    .await;

    evaluate(&mut viewer, "document.querySelector('#handback').click()").await;
    wait_for(&mut viewer, "document.querySelector('#note-dlg').open").await;
    viewer_chord(&mut viewer, 'v', 2).await;
    assert_eq!(evaluate(&mut remote, "pastes.length").await, 1);
    evaluate(
        &mut viewer,
        "document.querySelector('#note-dlg').close('ok')",
    )
    .await;
    wait_for(
        &mut viewer,
        "document.querySelector('#signal')?.dataset.state === 'view'",
    )
    .await;
    evaluate(
        &mut remote,
        &format!("box.value={menu_text}; box.select(); document.execCommand('copy')"),
    )
    .await;
    focus_desktop(&mut viewer).await;
    viewer_chord(&mut viewer, 'v', 2).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(evaluate(&mut remote, "pastes.length").await, 1);
    assert_eq!(
        evaluate(&mut viewer, "navigator.clipboard.readText()").await,
        unicode
    );

    browser.shutdown().await;
    compositor.shutdown().await;
    desktop.close().await;
}
