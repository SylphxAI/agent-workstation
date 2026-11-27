use super::*;
use tokio::net::TcpListener;

#[tokio::test]
async fn modal_interrupts_input_without_reply_and_requires_explicit_decision() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        for (kind, accept, text) in [
            ("alert", true, None),
            ("confirm", false, None),
            ("prompt", true, Some("")),
            ("beforeunload", false, None),
        ] {
            let request: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Input.dispatchMouseEvent");
            let opening = |session, kind| {
                json!({"method":"Page.javascriptDialogOpening","sessionId":session,
                "params":{"type":kind,"message":"fixture — message\n", "defaultPrompt":if kind == "prompt" {"default"} else {""},
                "url":"https://example.test/", "hasBrowserHandler":false}})
            };
            for event in [opening("other-page", "prompt"), opening("page", kind)] {
                ws.send(Message::text(event.to_string())).await.unwrap();
            }
            // Simulate Chromium withholding the input reply until the modal closes.
            let decision: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(decision["method"], "Page.handleJavaScriptDialog");
            assert_eq!(decision["sessionId"], "page");
            assert_eq!(decision["params"]["accept"], accept);
            assert_eq!(
                decision["params"].get("promptText").and_then(Value::as_str),
                text
            );
            for frame in [
                json!({"method":"Page.javascriptDialogClosed","sessionId":"page","params":{"userInput": "not queued"}}),
                json!({"id":request["id"], "result":{}}),
                json!({"id":decision["id"], "result":{}}),
            ] {
                ws.send(Message::text(frame.to_string())).await.unwrap();
            }
        }
        while ws.next().await.is_some() {}
    });
    let mut cdp = Cdp::connect(&format!("ws://{addr}")).await.unwrap();
    cdp.session_id = Some("page".into());
    for (kind, accept, text) in [
        ("alert", true, None),
        ("confirm", false, None),
        ("prompt", true, Some("")),
        ("beforeunload", false, None),
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            cdp.call("Input.dispatchMouseEvent", json!({})),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Error::DialogOpen)));
        let dialog = cdp.pending_dialog().unwrap();
        assert_eq!(dialog.kind, kind);
        assert_eq!(dialog.message, "fixture — message\n");
        assert_eq!(
            dialog.default_prompt,
            if kind == "prompt" { "default" } else { "" }
        );
        assert!(!dialog.has_browser_handler);
        assert!(matches!(
            cdp.call("Input.dispatchMouseEvent", json!({})).await,
            Err(Error::DialogOpen)
        ));
        cdp.handle_dialog(accept, text).await.unwrap();
        assert!(cdp.pending_dialog().is_none());
        assert!(matches!(
            cdp.handle_dialog(false, None).await,
            Err(Error::NoActiveDialog)
        ));
        assert!(cdp.events.is_empty());
    }
    drop(cdp);
    server.await.unwrap();
}
