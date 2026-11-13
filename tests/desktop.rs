//! Headful integration on a real headless-Wayland desktop.
//!
//! Runs only with `AGENT_WORKSTATION_HEADFUL=1`. Needs `sway`, `wayvnc` and a
//! Chromium-family browser (`AGENT_WORKSTATION_BROWSER`, default
//! `/usr/bin/chromium`); `AGENT_WORKSTATION_BROWSER_ARGS` adds browser flags.

use std::time::Duration;

use agent_workstation::{
    BrowserConfig, Button, DesktopSession, Error, Profile, Secret, SecretResolver, SessionConfig,
    TakeoverConfig,
};
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
</script>
</body></html>"#;

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

    // Exercise native input immediately after startup, before any page/CDP
    // operation that might incidentally give the browser time to get focus.
    s.key("ctrl+l").await.unwrap();
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

    // Desktop operations on the same window: click the email field by its
    // on-screen position and type through the virtual keyboard.
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
    s.snapshot().await.unwrap();
    s.key("ctrl+l").await.unwrap();
    s.type_text(&format!("{url}?typed=1\n")).await.unwrap();
    expect_url(&s, &format!("{url}?typed=1")).await;
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
