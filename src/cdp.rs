//! Chrome DevTools Protocol client over one WebSocket.
//!
//! Minimal core: connect to a browser- or page-level endpoint and issue
//! commands; events that arrive while waiting for a reply are buffered and
//! readable through [`Cdp::next_event`].

use std::collections::VecDeque;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{Error, Result};

/// Default wait for one command's reply.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// One CDP connection.
pub struct Cdp {
    endpoint: String,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    session_id: Option<String>,
    events: VecDeque<Value>,
    healthy: bool,
}

impl std::fmt::Debug for Cdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cdp")
            .field("session_id", &self.session_id)
            .field("healthy", &self.healthy)
            .finish_non_exhaustive()
    }
}

impl Cdp {
    /// Connect to a `ws://` endpoint.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        Self::connect_bounded(endpoint, None).await
    }

    pub(crate) async fn connect_bounded(
        endpoint: &str,
        max_message: Option<usize>,
    ) -> Result<Self> {
        let config = max_message.map(|limit| {
            WebSocketConfig::default()
                .max_message_size(Some(limit))
                .max_frame_size(Some(limit))
        });
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(endpoint, config, false),
        )
        .await
        .map_err(|_| Error::Timeout("CDP connect".into()))?
        .map_err(|e| Error::protocol(format!("CDP connect: {e}")))?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            ws,
            next_id: 0,
            session_id: None,
            events: VecDeque::new(),
            healthy: true,
        })
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Whether the socket is still usable.
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    /// Flat session commands go to (set by [`Cdp::attach`]).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Send one command and wait for its reply's `result`.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let session = self.session_id.clone();
        self.call_in(session.as_deref(), method, params, CALL_TIMEOUT)
            .await
    }

    /// Send one command on an explicit flat session (`None` = browser).
    pub async fn call_in(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let webauthn = method.starts_with("WebAuthn.");
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session_id {
            msg["sessionId"] = json!(s);
        }
        if let Err(e) = self.ws.send(Message::Text(msg.to_string().into())).await {
            self.healthy = false;
            return Err(if webauthn {
                Error::protocol(format!("{method}: send failed"))
            } else {
                Error::protocol(format!("{method}: send failed: {e}"))
            });
        }
        let wait = async {
            loop {
                let v = self.read_one().await.map_err(|e| {
                    if webauthn {
                        Error::protocol(format!("{method}: could not read reply"))
                    } else {
                        e
                    }
                })?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = v.get("error") {
                        if webauthn {
                            // A backend may echo command arguments in diagnostic text.
                            let code = err["code"].as_i64().unwrap_or_default();
                            return Err(Error::protocol(format!("{method}: CDP error ({code})")));
                        }
                        let text = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error");
                        return Err(Error::protocol(format!("{method}: {text}")));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                if v.get("method").is_some() {
                    self.events.push_back(v);
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| Error::Timeout(format!("{method} reply")))?
    }

    /// Next event (buffered first), waiting up to `timeout`.
    /// Private keys are omitted from WebAuthn credential events.
    pub async fn next_event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        if let Some(v) = self.events.pop_front() {
            return Ok(Some(v));
        }
        match tokio::time::timeout(timeout, async {
            loop {
                let v = self.read_one().await?;
                if v.get("method").is_some() {
                    return Ok::<_, Error>(v);
                }
            }
        })
        .await
        {
            Ok(r) => r.map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Consume the matching destruction event without discarding unrelated
    /// events. Target discovery must be enabled before requesting the close.
    pub async fn wait_target_destroyed(
        &mut self,
        target_id: &str,
        timeout: Duration,
    ) -> Result<()> {
        let matches = |v: &Value| {
            v["method"] == "Target.targetDestroyed" && v["params"]["targetId"] == target_id
        };
        if let Some(index) = self.events.iter().position(matches) {
            self.events.remove(index);
            return Ok(());
        }
        tokio::time::timeout(timeout, async {
            loop {
                let event = self.read_one().await?;
                if matches(&event) {
                    return Ok(());
                }
                if event.get("method").is_some() {
                    self.events.push_back(event);
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout("target destruction".into()))?
    }

    /// Attach to a page target with a flat session; later [`Cdp::call`]s go to it.
    pub async fn attach(&mut self, target_id: &str) -> Result<String> {
        let r = self
            .call_in(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                CALL_TIMEOUT,
            )
            .await?;
        let sid = r
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::protocol("attachToTarget returned no sessionId"))?
            .to_string();
        self.session_id = Some(sid.clone());
        Ok(sid)
    }

    /// Detach from the current page session (commands go to the browser).
    pub fn detach(&mut self) {
        self.session_id = None;
    }

    /// Page targets, as returned by `Target.getTargets`.
    pub async fn pages(&mut self) -> Result<Vec<Value>> {
        let r = self
            .call_in(None, "Target.getTargets", json!({}), CALL_TIMEOUT)
            .await?;
        Ok(r.get("targetInfos")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn read_one(&mut self) -> Result<Value> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let mut value: Value = serde_json::from_str(&t)
                        .map_err(|e| Error::protocol(format!("bad CDP frame: {e}")))?;
                    // WebAuthn emits key-bearing credential events even during ordinary
                    // page evaluations. Export keys only through explicit command replies.
                    if value["method"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("WebAuthn."))
                        && let Some(credential) = value
                            .get_mut("params")
                            .and_then(|params| params.get_mut("credential"))
                            .and_then(Value::as_object_mut)
                    {
                        credential.remove("privateKey");
                        credential.remove("cmtgKeys");
                    }
                    return Ok(value);
                }
                Some(Ok(Message::Close(_))) | None => {
                    self.healthy = false;
                    return Err(Error::protocol("CDP socket closed"));
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    self.healthy = false;
                    return Err(Error::protocol(format!("CDP socket: {e}")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn destruction_wait_matches_identity_and_preserves_other_events() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            for event in [
                json!({"method":"Target.targetDestroyed","params":{"targetId":"other"}}),
                json!({"method":"Page.frameNavigated","params":{}}),
                json!({"method":"Target.targetDestroyed","params":{"targetId":"closed"}}),
            ] {
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            // Keep the transport alive until the client is done.
            while ws.next().await.is_some() {}
        });
        let mut cdp = Cdp::connect(&format!("ws://{address}")).await.unwrap();
        cdp.wait_target_destroyed("closed", Duration::from_secs(1))
            .await
            .unwrap();
        let first = cdp.next_event(Duration::ZERO).await.unwrap().unwrap();
        assert_eq!(first["params"]["targetId"], "other");
        assert_eq!(
            cdp.next_event(Duration::ZERO).await.unwrap().unwrap()["method"],
            "Page.frameNavigated"
        );
        cdp.events
            .push_back(json!({"method":"Target.targetDestroyed","params":{"targetId":"buffered"}}));
        cdp.wait_target_destroyed("buffered", Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            cdp.wait_target_destroyed("absent", Duration::from_millis(10))
                .await,
            Err(Error::Timeout(_))
        ));
        assert!(cdp.is_healthy());
        drop(cdp);
        server.abort();
    }

    #[test]
    fn websocket_payload_logging_is_disabled_at_compile_time() {
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Info);
    }

    #[tokio::test]
    async fn webauthn_command_errors_and_buffered_events_do_not_expose_keys() {
        let synthetic_key = "AQIDBA==";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let command = ws.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(command.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "WebAuthn.addCredential");
            assert_eq!(request["sessionId"], "page-session");
            for frame in [
                json!({"method":"WebAuthn.credentialAdded","sessionId":"page-session","params":{
                    "authenticatorId":"auth", "credential":{"credentialId":"AQ==", "privateKey":synthetic_key,"cmtgKeys":[synthetic_key]}
                }}),
                json!({"id":request["id"],"sessionId":"page-session","error":{
                    "code":-32602,"message":format!("invalid key: {synthetic_key}"),"data":synthetic_key
                }}),
            ] {
                ws.send(Message::Text(frame.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let mut cdp = Cdp::connect(&format!("ws://{address}")).await.unwrap();
        let error = cdp
            .call_in(
                Some("page-session"),
                "WebAuthn.addCredential",
                json!({
                    "authenticatorId":"auth","credential":{"privateKey":synthetic_key}
                }),
                CALL_TIMEOUT,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "browser protocol: WebAuthn.addCredential: CDP error (-32602)"
        );
        assert!(!format!("{error:?}").contains(synthetic_key));
        let event = cdp.next_event(Duration::ZERO).await.unwrap().unwrap();
        assert_eq!(
            event["params"]["credential"],
            json!({"credentialId":"AQ=="})
        );
        assert!(!format!("{cdp:?}").contains(synthetic_key));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn destruction_wait_propagates_transport_loss() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.close(None).await.unwrap();
        });
        let mut cdp = Cdp::connect(&format!("ws://{address}")).await.unwrap();
        assert!(matches!(
            cdp.wait_target_destroyed("closed", Duration::from_secs(1))
                .await,
            Err(Error::Protocol(_))
        ));
        assert!(!cdp.is_healthy());
        server.await.unwrap();
    }
}
