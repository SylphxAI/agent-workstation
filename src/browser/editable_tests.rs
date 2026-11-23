use super::*;
use crate::takeover::{InputContext, InputPurpose};
use crate::{BrowserConfig, Button, DesktopSession, Profile, SessionConfig};

fn pixel(image: &crate::Frame, x: i32, y: i32) -> [u8; 3] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.rgba[i..i + 3].try_into().unwrap()
}

fn page_url(html: &str) -> String {
    use base64::Engine;
    format!(
        "data:text/html;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(html)
    )
}

async fn expect_context(
    endpoint: &str,
    rect: [i32; 4],
    accept: impl Fn(&InputContext) -> bool,
) -> InputContext {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let context = editable_context(endpoint, rect).await;
        if context.as_ref().is_some_and(&accept) {
            return context.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "editable probe did not observe expected state; last active / rects: {:?}",
            context.as_ref().map(|c| (c.active, &c.rects))
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn headful_editable_context_matches_painted_desktop_and_trusted_focus() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        return;
    }
    let profile = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let mut browser = BrowserConfig::new(
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into()),
    );
    if let Ok(extra) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        browser.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    let mut config = SessionConfig::new(
        Profile::new("editable-fixture", profile.path()),
        browser,
        runtime.path(),
    );
    config.fingerprint_defaults.screen_width = 1024;
    config.fingerprint_defaults.screen_height = 720;
    config.takeover = None;
    let desktop = DesktopSession::start(config).await.unwrap();
    let agent = desktop.lease("editable-fixture").unwrap();
    let (port, path) =
        read_port_file(&profile.path().join("user-data/DevToolsActivePort")).unwrap();
    let endpoint = format!("ws://127.0.0.1:{port}{path}");
    let window = agent
        .desktop_windows()
        .await
        .unwrap()
        .into_iter()
        .find(|w| w.focused)
        .unwrap()
        .rect;
    let window = [
        window.x,
        window.y,
        window.width as i32,
        window.height as i32,
    ];

    let main = page_url(
        r#"<!doctype html>
<style>
    body { margin: 0; }
    input { appearance: none; position: absolute; border: 0; padding: 0;
        width: 100px; height: 28px; background: #02fa76; }
</style>
<input type=email style="left:40px;top:32px" value="synthetic-private">
<input readonly style="left:40px;top:75px">
<fieldset disabled><input style="left:40px;top:120px"></fieldset>
<input style="left:40px;top:165px;opacity:0">
<input style="left:40px;top:210px"><div style="position:absolute;left:40px;top:210px;width:100px;height:28px;background:red"></div>
<input style="left:40px;top:255px" inputmode=none>
<div style="position:absolute;left:200px;top:32px;width:60px;height:20px;overflow:hidden"><input style="left:20px;top:10px"></div>
<iframe style="position:absolute;left:400px;top:80px;width:200px;height:100px;border:4px solid #444" srcdoc="<body style='margin:0'><input type=url style='position:absolute;left:35px;top:25px;width:50px;height:20px;padding:0;border:0;background:#02fa76'></body>"></iframe>
<div id=shadow style="position:absolute;left:350px;top:30px"></div>
<div contenteditable style="position:absolute;left:550px;top:30px;width:80px;height:20px">Edit me</div>
<script>
    const root = document.querySelector('#shadow').attachShadow({mode:'open'});
    root.innerHTML = '<input inputmode=decimal style="display:block;width:60px;height:24px;padding:0;border:0;background:#02fa76">';
    // The observer must not use page-world prototypes.
    Document.prototype.hasFocus = () => false;
    Element.prototype.getClientRects = () => { throw Error('page patch'); };
</script>"#,
    );
    agent.navigate(&main).await.unwrap();
    let ctx = expect_context(&endpoint, window, |c| c.rects.len() == 5).await;
    let top = ctx.rects[0][1] - 32;
    assert!(top > 0, "expected real Chromium toolbar above the page");
    assert_eq!(
        ctx.rects,
        [
            [40, top + 32, 100, 28],
            [220, top + 42, 40, 10],
            [350, top + 30, 60, 24],
            [439, top + 109, 50, 20],
            [550, top + 30, 80, 20]
        ]
    );

    // Verify predicted geometry against a compositor screenshot, not a second
    // calculation from DOM rectangles. Probe away from the field's text.
    let deadline = Instant::now() + Duration::from_secs(3);
    let image = loop {
        let image = agent.screenshot().await.unwrap();
        // Wait for the field to be painted anywhere in its known column,
        // independently of the proposed y coordinate.
        if (0..image.height as i32).any(|y| pixel(&image, 132, y) == [2, 250, 118]) {
            break image;
        }
        assert!(Instant::now() < deadline, "fixture has not painted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(pixel(&image, 132, top + 34), [2, 250, 118]);
    assert_ne!(pixel(&image, 132, top + 31), [2, 250, 118]);
    assert_eq!(pixel(&image, 229, top + 48), [2, 250, 118]);
    assert_ne!(pixel(&image, 229, top + 52), [2, 250, 118]);

    for (x, y, purpose) in [
        (120, top + 44, InputPurpose::Email),
        (360, top + 42, InputPurpose::Decimal),
        (450, top + 119, InputPurpose::Url),
        (560, top + 40, InputPurpose::Text),
    ] {
        agent
            .click_at(f64::from(x), f64::from(y), Button::Left)
            .await
            .unwrap();
        let _ = expect_context(&endpoint, window, |c| c.active && c.purpose == purpose).await;
    }
    agent.key("ctrl+l").await.unwrap();
    let _ = expect_context(&endpoint, window, |c| !c.active && c.rects.len() == 5).await;

    // The observer follows a real tab switch without activating the agent's
    // old tab or disturbing the browser's native focus ownership.
    let second = page_url(
        "<!doctype html><input type=search style='position:absolute;left:600px;top:30px'>",
    );
    let second_id = agent.new_tab(&second).await.unwrap();
    let ctx = expect_context(&endpoint, window, |c| {
        c.rects.len() == 1 && c.rects[0][0] == 600
    })
    .await;
    agent
        .click_at(610.0, f64::from(ctx.rects[0][1] + 8), Button::Left)
        .await
        .unwrap();
    let _ = expect_context(&endpoint, window, |c| {
        c.active && c.purpose == InputPurpose::Search
    })
    .await;
    assert_eq!(agent.current_url().await.unwrap(), second);
    agent.close_tab(&second_id).await.unwrap();
    let _ = expect_context(&endpoint, window, |c| c.rects.len() == 5).await;
    assert!(editable_context(&endpoint, [0, 0, 40, 50]).await.is_none());
    desktop.close().await;
}
