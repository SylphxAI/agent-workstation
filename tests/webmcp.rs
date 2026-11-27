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
    let tool_ref = discover(web, name).await?;
    web.webmcp_call(&tool_ref, input, false, WEBMCP_DEFAULT_TIMEOUT_MS)
        .await
        .map(|r| r.value)
}

async fn discover(web: &mut Web, name: &str) -> agent_workstation::Result<String> {
    web.webmcp_list()
        .await?
        .tools
        .into_iter()
        .find(|tool| tool.name == name)
        .map(|tool| tool.tool_ref)
        .ok_or_else(|| Error::NotFound("fixture tool was not discovered".into()))
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
                .route("/form", axum::routing::get(|| async { Html(include_str!("fixtures/webmcp-form.html")) }))
                .route(
                    "/plain",
                    axum::routing::get(|| async { Html("<p>Plain content</p><input id=query><button id=go onclick='window.clicked=query.value'>Go</button><script>Object.defineProperty(document, 'modelContext', {value: undefined})</script>") }),
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
        "--enable-blink-features=WebMCP,DeclarativeWebMCP".into(),
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
    assert!(web.webmcp_list().await.unwrap().tools.is_empty());
    assert_eq!(web.read().await.unwrap(), "Plain content\n\nGo");
    assert!(matches!(
        call(&mut web, "echo", json!({})).await,
        Err(Error::NotFound(_))
    ));
    let snapshot = web.snapshot().await.unwrap();
    assert!(snapshot.text.contains("Plain content"));
    web.fill("#query", "still works").await.unwrap();
    web.click("#go").await.unwrap();
    assert_eq!(web.eval("window.clicked").await.unwrap(), "still works");

    web.navigate(&format!("{origin}/native")).await.unwrap();
    web.eval("window.ready").await.unwrap();
    let discovery = web.webmcp_list().await.unwrap();
    assert!(discovery.untrusted);
    assert_eq!(discovery.origin.as_deref(), Some(origin.as_str()));
    let tools = discovery.tools;
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
        Err(Error::StaleRef(_))
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
    let secret = discover(&mut web, "secret").await.unwrap();
    let error = web
        .webmcp_call(
            &secret,
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
    let gated = discover(&mut web, "gated").await.unwrap();
    assert_eq!(
        web.webmcp_call(&gated, json!({"text": "allowed"}), true, 5_000)
            .await
            .unwrap()
            .value,
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
    let slow = discover(&mut web, "slow").await.unwrap();
    assert!(matches!(
        web.webmcp_call(&slow, json!({}), false, 700).await,
        Err(Error::Timeout(_))
    ));
    observe(&mut observer, "effects.aborted", 1).await;
    {
        let mut pending = Box::pin(web.webmcp_call(&slow, json!({}), false, 5_000));
        tokio::select! {
            result = &mut pending => panic!("slow returned early: {result:?}"),
            _ = observe(&mut observer, "effects.started", 2) => {}
        }
        // Drop before the deadline; the callback must see the real abort independently.
    }
    observe(&mut observer, "effects.aborted", 2).await;

    let echo = discover(&mut web, "echo").await.unwrap();
    web.eval("unregister()").await.unwrap();
    let removed = web.webmcp_call(&echo, json!({}), false, 5_000).await;
    assert!(
        matches!(removed, Err(Error::NotFound(_) | Error::StaleRef(_))),
        "{removed:?}"
    );
    web.eval("registerAgain()").await.unwrap();
    let replaced = web.webmcp_call(&echo, json!({}), false, 5_000).await;
    assert!(
        matches!(replaced, Err(Error::NotFound(_) | Error::StaleRef(_))),
        "{replaced:?}"
    );
    assert_eq!(
        call(&mut web, "echo", json!({})).await.unwrap(),
        json!({"new":true})
    );
    web.navigate(&format!("{origin}/native")).await.unwrap();
    web.eval("window.ready").await.unwrap();
    web.eval("unregister()").await.unwrap();
    assert!(
        !web.webmcp_list()
            .await
            .unwrap()
            .tools
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

    let echo = discover(&mut web, "echo").await.unwrap();
    let second_tab = web.new_tab(&format!("{origin}/native")).await.unwrap();
    assert!(matches!(
        web.webmcp_call(&echo, json!({}), false, 5_000).await,
        Err(Error::StaleRef(_))
    ));
    web.close_tab(&second_tab).await.unwrap();
    let echo = discover(&mut web, "echo").await.unwrap();
    // Navigate externally, bypassing the Web controller's invalidation.
    observer
        .call(
            "Page.navigate",
            json!({"url":format!("{origin}/native?same-origin")}),
        )
        .await
        .unwrap();
    observe(
        &mut observer,
        "Number(location.search === '?same-origin')",
        1,
    )
    .await;
    assert!(matches!(
        web.webmcp_call(&echo, json!({}), false, 5_000).await,
        Err(Error::StaleRef(_))
    ));

    let echo = discover(&mut web, "echo").await.unwrap();
    let other_origin = origin.replace("127.0.0.1", "localhost");
    observer
        .call(
            "Page.navigate",
            json!({"url":format!("{other_origin}/native")}),
        )
        .await
        .unwrap();
    observe(
        &mut observer,
        "Number(location.hostname === 'localhost')",
        1,
    )
    .await;
    assert!(matches!(
        web.webmcp_call(&echo, json!({}), false, 5_000).await,
        Err(Error::StaleRef(_))
    ));

    web.navigate(&format!("{origin}/navigator")).await.unwrap();
    let discovery = web.webmcp_list().await.unwrap();
    assert!(discovery.untrusted);
    assert_eq!(discovery.origin.as_deref(), Some(origin.as_str()));
    let tools = discovery.tools;
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["ui.snapshot", "ui.query", "ui.act", "refuses", "oversized"]
    );
    let snapshot = &tools[0];
    assert_eq!(snapshot.annotations["readOnlyHint"], true);
    assert_eq!(snapshot.annotations["untrustedContentHint"], false);
    assert_eq!(tools[2].annotations["consequentialHint"], true);
    let result = web
        .webmcp_call(&snapshot.tool_ref, json!({}), false, 5_000)
        .await
        .unwrap();
    assert!(result.untrusted);
    assert_eq!(result.origin, origin);
    let result = result.value;
    assert!(matches!(
        web.webmcp_call(
            &snapshot.tool_ref,
            json!({"input":"é".repeat(9000)}),
            false,
            5_000
        )
        .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        web.webmcp_call(&tools[4].tool_ref, json!({}), false, 5_000)
            .await,
        Err(Error::Refused(_))
    ));
    let refusal = web
        .webmcp_call(&tools[3].tool_ref, json!({}), false, 5_000)
        .await
        .unwrap_err();
    assert_eq!(refusal.code(), "refused");
    assert!(!refusal.to_string().contains("fixture-secret"));
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
    assert_webmcp_dialogs(&mut web).await;
    web.navigate(&format!("{origin}/plain")).await.unwrap();
    assert!(web.webmcp_list().await.unwrap().tools.is_empty());
    web.navigate(&format!("{origin}/form")).await.unwrap();
    web.eval("new Promise(resolve => { const frame = document.createElement('iframe'); frame.src = '/native'; frame.onload = async () => { await frame.contentWindow.ready; resolve(true); }; document.body.append(frame); })").await.unwrap();
    let discovery = web.webmcp_list().await.unwrap();
    assert_eq!(
        discovery.tools.len(),
        1,
        "do not expose iframe registrations"
    );
    let form = discovery
        .tools
        .iter()
        .find(|t| t.name == "catalog_search")
        .expect("native declarative form must be registered");
    assert_eq!(form.description, "Search the catalog");
    assert_eq!(form.input_schema["properties"]["query"]["type"], "string");
    let result = web
        .webmcp_call(&form.tool_ref, json!({"query":"café"}), false, 5_000)
        .await
        .unwrap();
    assert!(result.untrusted);
    assert_eq!(result.value, json!({"query":"café", "agentInvoked":true}));
    browser.shutdown().await;
    server.abort();
}

async fn assert_webmcp_dialogs(web: &mut Web) {
    async fn expect_dialog<T: std::fmt::Debug>(
        operation: impl Future<Output = agent_workstation::Result<T>>,
    ) {
        let result = timeout(Duration::from_secs(3), operation)
            .await
            .expect("WebMCP waited for the blocked renderer");
        assert!(matches!(result, Err(Error::DialogOpen)), "{result:?}");
    }

    web.eval(
        r#"navigator.modelContext.provideContext({tools: [{
            name: 'opens_alert', description: 'Open a modal',
            execute: () => { alert('tool modal'); return true; },
        }]})"#,
    )
    .await
    .unwrap();
    let tool = discover(web, "opens_alert").await.unwrap();
    // The tool itself opens the modal, so no timer or scheduling guess is needed.
    expect_dialog(web.webmcp_call(&tool, json!({}), false, 5_000)).await;
    let dialog = web.pending_dialog().await.unwrap().unwrap();
    assert_eq!(dialog.kind, "alert");
    assert_eq!(dialog.message, "tool modal");

    // Neither operation may touch the blocked renderer or dismiss the modal.
    expect_dialog(web.webmcp_call(&tool, json!({}), false, 5_000)).await;
    expect_dialog(web.webmcp_list()).await;
    web.handle_dialog(true, None).await.unwrap();
    assert!(web.pending_dialog().await.unwrap().is_none());

    web.eval(
        r#"(() => {
            const context = navigator.modelContext;
            const getTools = context.getTools;
            context.getTools = () => {
                context.getTools = getTools;
                prompt('discovery modal');
                return getTools();
            };
        })()"#,
    )
    .await
    .unwrap();
    expect_dialog(web.webmcp_list()).await;
    let dialog = web.pending_dialog().await.unwrap().unwrap();
    assert_eq!(dialog.kind, "prompt");
    assert_eq!(dialog.message, "discovery modal");
    web.handle_dialog(false, None).await.unwrap();
    assert!(web.pending_dialog().await.unwrap().is_none());
    let recovered = call(web, "ui.snapshot", json!({})).await.unwrap();
    assert_eq!(recovered["role"], "window");
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
