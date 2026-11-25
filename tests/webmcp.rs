//! Real, script-enabled headless Chromium; the native fixture uses actual WebIDL registrations.
#![cfg(target_os = "linux")]

use std::time::Duration;

use agent_workstation::browser::{Browser, Web};
use agent_workstation::cdp::Cdp;
use agent_workstation::{BrowserConfig, Error, Fingerprint, WEBMCP_DEFAULT_TIMEOUT_MS};
use axum::Router;
use axum::response::Html;
use serde_json::{Value, json};
use tokio::time::timeout;

async fn call(web: &mut Web, name: &str, input: Value) -> agent_workstation::Result<Value> {
    web.webmcp_call(name, input, false, WEBMCP_DEFAULT_TIMEOUT_MS)
        .await
}

#[tokio::test]
async fn real_headless_chromium_webmcp() {
    let Ok(binary) = std::env::var("AGENT_WORKSTATION_WEBMCP_BROWSER") else {
        eprintln!("set AGENT_WORKSTATION_WEBMCP_BROWSER to run the real Chromium WebMCP test");
        return;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/native",
                    axum::routing::get(|| async { Html(include_str!("fixtures/webmcp.html")) }),
                )
                .route(
                    "/navigator",
                    axum::routing::get(|| async {
                        Html(include_str!("fixtures/webmcp-navigator.html"))
                    }),
                )
                .route(
                    "/plain",
                    axum::routing::get(|| async { Html("<p>Plain content</p><script>Object.defineProperty(document, 'modelContext', {value: undefined})</script>") }),
                ),
        )
        .await
        .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("chrome.log");
    let mut browser = BrowserConfig::new(binary);
    browser.extra_args = vec![
        "--headless".into(),
        "--disable-gpu".into(),
        "--enable-blink-features=WebMCP".into(),
    ];
    // Only opt out for disposable CI/sandbox environments whose namespaces
    // disallow the browser's own sandbox. The normal launch remains unchanged.
    if std::env::var_os("AGENT_WORKSTATION_WEBMCP_NO_SANDBOX").is_some() {
        browser.extra_args.push("--no-sandbox".into());
    }
    let browser = Browser::launch(
        &browser,
        &[],
        temp.path(),
        &Fingerprint::generate("webmcp", &Default::default()),
        None,
        &log,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}: {}", std::fs::read_to_string(log).unwrap_or_default()));
    let mut web = Web::connect(browser.endpoint()).await.unwrap();

    web.navigate(&format!("{origin}/plain")).await.unwrap();
    assert!(web.webmcp_list().await.unwrap().is_empty());
    assert_eq!(web.read().await.unwrap(), "Plain content");
    assert!(matches!(
        call(&mut web, "echo", json!({})).await,
        Err(Error::NotFound(_))
    ));

    web.navigate(&format!("{origin}/native")).await.unwrap();
    web.eval("window.ready").await.unwrap();
    let tools = web.webmcp_list().await.unwrap();
    assert!(tools.iter().all(|tool| tool.name != "hidden"));
    let tool = tools.iter().find(|tool| tool.name == "gated").unwrap();
    assert_eq!(tool.description, "Run gated");
    assert_eq!(tool.annotations["agent_confirm"], true);
    assert_eq!(tool.input_schema["properties"]["text"]["type"], "string");
    assert!(
        !serde_json::to_string(&tools)
            .unwrap()
            .contains("fixture-secret")
    );
    let input = json!({"nested": ["'\"\n", {"__proto__": {"injected": true}}]});
    assert_eq!(
        call(&mut web, "echo", input.clone()).await.unwrap(),
        json!({"input": input, "realSignal": true})
    );
    assert_eq!(
        call(&mut web, "plain_text", json!({})).await.unwrap(),
        "false"
    );
    assert!(matches!(
        web.webmcp_call("hidden", json!({}), true, 5_000).await,
        Err(Error::NotFound(_))
    ));
    let error = call(
        &mut web,
        "gated",
        json!({"text": "fixture-secret", "confirmed": true}),
    )
    .await
    .unwrap_err();
    assert!(!format!("{error:?}").contains("fixture-secret"));
    assert!(
        matches!(error, Error::WebMcpConfirmation { tool, description, arguments }
        if tool == "gated" && description == "Run gated" && json!(arguments) == json!({"text": "[redacted]"}))
    );
    let error = web
        .webmcp_call(
            "secret",
            json!({"credential": "fixture-secret"}),
            true,
            5_000,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Refused(_)));
    assert!(!format!("{error:?}").contains("fixture-secret"));
    assert!(
        !format!(
            "{:?}",
            call(&mut web, "throws", json!({})).await.unwrap_err()
        )
        .contains("fixture-secret")
    );
    assert_eq!(
        web.eval("effects").await.unwrap(),
        json!({"hidden":0,"confirmed":0,"secret":0,"clicks":0,"started":0,"aborted":0})
    );
    assert_eq!(
        web.webmcp_call("gated", json!({"text": "allowed"}), true, 5_000)
            .await
            .unwrap(),
        json!({"ok":true})
    );
    assert_eq!(web.eval("effects.confirmed").await.unwrap(), 1);

    // Keep a second CDP client to observe the callback while the call holds Web.
    let tab = web
        .tabs()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.active)
        .unwrap();
    let mut observer = Cdp::connect(browser.endpoint()).await.unwrap();
    observer.attach(&tab.id).await.unwrap();
    assert!(matches!(
        web.webmcp_call("slow", json!({}), false, 700).await,
        Err(Error::Timeout(_))
    ));
    observe(&mut observer, "effects.aborted", 1).await;
    {
        let mut pending = Box::pin(web.webmcp_call("slow", json!({}), false, 5_000));
        tokio::select! {
            result = &mut pending => panic!("slow returned early: {result:?}"),
            _ = observe(&mut observer, "effects.started", 2) => {}
        }
        // Drop before the deadline; the callback must see the real abort independently.
    }
    observe(&mut observer, "effects.aborted", 2).await;

    web.eval("unregister()").await.unwrap();
    assert!(
        !web.webmcp_list()
            .await
            .unwrap()
            .iter()
            .any(|t| t.name == "echo")
    );
    assert!(matches!(
        call(&mut web, "echo", json!({})).await,
        Err(Error::NotFound(_))
    ));
    web.eval("registerAgain()").await.unwrap();
    assert_eq!(
        call(&mut web, "echo", json!({})).await.unwrap(),
        json!({"new":true})
    );

    web.navigate(&format!("{origin}/navigator")).await.unwrap();
    let tools = web.webmcp_list().await.unwrap();
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["ui.snapshot", "ui.query", "ui.act"]
    );
    let result = call(&mut web, "ui.snapshot", json!({})).await.unwrap();
    assert!(!result.to_string().contains("fixture-"));
    assert_eq!(
        result["children"],
        json!([
            {"ref":"save","role":"button","label":"Save"},
            {"ref":"password","secret":true,"value":"[redacted]"}, "[redacted]"
        ])
    );
    assert_eq!(
        call(&mut web, "ui.query", json!({"ref":"private"}))
            .await
            .unwrap(),
        json!([])
    );
    assert!(matches!(
        call(&mut web, "invisible", json!({})).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        call(&mut web, "ui.act", json!({"ref":"save","action":"click"}))
            .await
            .unwrap(),
        json!({"ok":true})
    );
    assert_eq!(
        call(
            &mut web,
            "ui.act",
            json!({"ref":"locked","action":"click","confirmed":true})
        )
        .await
        .unwrap()["error"]["code"],
        "confirmation_required"
    );
    assert_eq!(
        web.eval("counter").await.unwrap(),
        json!({"clicks":1,"documentCalls":0})
    );
    assert!(matches!(
        call(&mut web, "echo", json!({})).await,
        Err(Error::NotFound(_))
    ));
    web.navigate(&format!("{origin}/plain")).await.unwrap();
    assert!(web.webmcp_list().await.unwrap().is_empty());
    browser.shutdown().await;
    server.abort();
}

async fn observe(cdp: &mut Cdp, expression: &str, expected: u64) {
    timeout(Duration::from_secs(2), async {
        loop {
            let r = cdp
                .call(
                    "Runtime.evaluate",
                    json!({"expression":expression,"returnByValue":true}),
                )
                .await
                .unwrap();
            if r["result"]["value"] == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("page did not observe {expression} = {expected}"));
}
