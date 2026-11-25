//! Deterministic CDP boundary tests: no browser, timing sleeps or external pages.
use super::*;
use crate::{ControlGate, HandBack, Handoff, Holder};
use futures::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Truncated,
    Replaced,
    Unreadable,
    WrongTarget,
    NewCycle,
    NewLease,
    Closed,
}

async fn fixture(gate: Arc<ControlGate>, fault: Fault) -> (Mutex<Web>, Arc<StdMutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let recorded = calls.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut frames = 0;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let req: Value = serde_json::from_str(&text).unwrap();
            recorded.lock().unwrap().push(req.clone());
            let result = match req["method"].as_str().unwrap() {
                "Target.attachToTarget" => json!({"sessionId":"session"}),
                "Target.getTargetInfo" => {
                    json!({"targetInfo":{"targetId": if matches!(fault, Fault::WrongTarget) {"other"} else {"page"}}})
                }
                "Page.getFrameTree" => {
                    frames += 1;
                    json!({"frameTree":{"frame":{"id":"frame", "loaderId": if frames > 1 && matches!(fault, Fault::Replaced) {"next"} else {"loader"}, "url":"https://fixture.invalid/"}}})
                }
                "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                "Runtime.callFunctionOn" => {
                    match fault {
                        Fault::NewCycle => {
                            gate.release(None);
                            gate.take(None);
                        }
                        Fault::NewLease => {
                            gate.release_lease("a");
                            gate.acquire("a").unwrap();
                        }
                        Fault::Closed => gate.close(),
                        _ => {}
                    }
                    if matches!(fault, Fault::Unreadable) {
                        json!({"exceptionDetails":{"text":"no document"}})
                    } else {
                        json!({"result":{"value":{"target_id":"", "url":"https://fixture.invalid/", "forms":[{"tag":"input","id":"token","name":"proof","value_nonempty":true}],"selectors":[{"matched":true,"visible":false}],"truncated":matches!(fault, Fault::Truncated)}}})
                    }
                }
                method => panic!("unexpected supervisor command: {method}"),
            };
            if ws
                .send(Message::Text(
                    json!({"id":req["id"],"result":result}).to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut cdp = Cdp::connect(&endpoint).await.unwrap();
    cdp.attach("page").await.unwrap();
    (
        Mutex::new(Web {
            cdp,
            active: Some("page".into()),
            refs: None,
            dropped: None,
            nonce: nonce().unwrap(),
            snapshots: 0,
            held: vec![],
            webauthn_proxy_active: Arc::new(AtomicBool::new(false)),
        }),
        calls,
    )
}

fn gate() -> Arc<ControlGate> {
    let g = ControlGate::new();
    g.acquire("a").unwrap();
    g.take(None);
    g
}

#[tokio::test]
async fn handoff_read_is_fixed_scoped_and_does_not_unlock_until_confirmed() {
    let g = gate();
    let (web, calls) = fixture(g.clone(), Fault::None).await;
    let h = Handoff::new(&g, &web, g.handoff("a").unwrap());
    let obs = h.observe(&["#challenge"]).await.unwrap();
    assert!(matches!(g.check("a"), Err(Error::HumanInControl { .. })));
    let e = obs.evidence();
    assert_eq!(e.target_id, "page");
    assert!(e.forms[0].value_nonempty);
    assert!(e.selectors[0].matched && !e.selectors[0].visible);
    {
        let recorded = calls.lock().unwrap();
        let methods: Vec<_> = recorded
            .iter()
            .map(|v| v["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "Target.attachToTarget",
                "Target.getTargetInfo",
                "Page.getFrameTree",
                "Page.createIsolatedWorld",
                "Runtime.callFunctionOn",
                "Page.getFrameTree"
            ]
        );
        for r in recorded.iter().skip(1) {
            assert_eq!(r["sessionId"], "session");
        }
        let read = &recorded[4]["params"];
        assert_eq!(read["functionDeclaration"], HANDOFF_READ);
        assert_eq!(read["executionContextId"], 42);
        assert_eq!(read["arguments"], json!([{"value":["#challenge"]}]));
    }
    obs.complete().unwrap();
    assert_eq!(g.take_hand_back(), Some(HandBack::Confirmed));
    assert!(g.check("a").is_ok());
    assert!(matches!(h.observe(&[]).await, Err(Error::StaleHandoff)));
}

#[tokio::test]
async fn handoff_read_fails_closed_on_partial_or_changed_evidence() {
    for fault in [
        Fault::Truncated,
        Fault::Replaced,
        Fault::Unreadable,
        Fault::WrongTarget,
        Fault::NewCycle,
        Fault::NewLease,
        Fault::Closed,
    ] {
        let g = gate();
        let (web, _) = fixture(g.clone(), fault).await;
        let h = Handoff::new(&g, &web, g.handoff("a").unwrap());
        let result = h.observe(&["#challenge"]).await;
        match fault {
            Fault::Truncated => {
                assert!(matches!(result.unwrap().complete(), Err(Error::Refused(_))))
            }
            Fault::Closed => assert!(matches!(result, Err(Error::Closed))),
            Fault::NewCycle | Fault::NewLease => {
                assert!(matches!(result, Err(Error::StaleHandoff)))
            }
            _ => assert!(result.is_err(), "{fault:?}"),
        }
        assert_eq!(
            g.hand_backs(),
            usize::from(matches!(fault, Fault::NewCycle)) as u64
        );
        assert!(matches!(g.holder(), Holder::Human { .. }));
    }
}

#[tokio::test]
async fn handoff_waiting_for_web_cannot_read_a_new_cycle_or_closed_session() {
    for close in [false, true] {
        let g = gate();
        let (web, calls) = fixture(g.clone(), Fault::None).await;
        let h = Handoff::new(&g, &web, g.handoff("a").unwrap());
        let lock = web.lock().await;
        let pending = h.observe(&[]);
        tokio::pin!(pending);
        assert!(futures::poll!(pending.as_mut()).is_pending());
        if close {
            g.close();
        } else {
            g.release(None);
            g.take(None);
        }
        drop(lock);
        assert!(matches!(
            pending.await,
            Err(Error::Closed | Error::StaleHandoff)
        ));
        assert_eq!(calls.lock().unwrap().len(), 1); // setup only; no evidence read
    }
}

#[tokio::test]
async fn handoff_invalid_selector_bounds_do_not_issue_cdp_reads() {
    let g = gate();
    let (web, calls) = fixture(g.clone(), Fault::None).await;
    let h = Handoff::new(&g, &web, g.handoff("a").unwrap());
    for selectors in [vec![""], vec!["a"; 33]] {
        assert!(matches!(
            h.observe(&selectors).await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert!(matches!(
        h.observe(&[&"a".repeat(1025)]).await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
}
