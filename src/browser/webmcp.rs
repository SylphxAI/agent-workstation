//! WebMCP consumer over the active tab's main-world ModelContext.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

use super::Web;
use crate::cdp::Cdp;
use crate::{Error, Result};

const BRIDGE: &str = include_str!("webmcp.js");
/// Default WebMCP deadline in milliseconds.
pub const WEBMCP_DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Maximum WebMCP deadline in milliseconds.
pub const WEBMCP_MAX_TIMEOUT_MS: u64 = 20_000;

/// A tool exposed by the active document, not an authorization grant.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebMcpTool {
    /// The page-defined tool name.
    pub name: String,
    /// The page-defined description (empty for a secret-bearing tool).
    pub description: String,
    /// The page-defined JSON Schema, or `null` if absent or secret-bearing.
    pub input_schema: Value,
    /// Recognized annotations and safety markers.
    pub annotations: Value,
}

fn timeout_error() -> Error {
    Error::Timeout("WebMCP".into())
}

fn invocation_key() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("__webmcp_{hex}"))
}

fn evaluate(payload: Value) -> Value {
    // Parse arguments as JSON data, not an object literal (notably __proto__).
    let json_string = Value::String(payload.to_string());
    json!({
        "expression": format!("{BRIDGE}(JSON.parse({json_string}))"),
        "awaitPromise": true,
        "returnByValue": true,
        "silent": true,
    })
}

fn decode(reply: Value) -> Result<Value> {
    let status = &reply["result"]["value"];
    match status["status"].as_str() {
        Some("ok") => status.get("value").cloned().ok_or_else(failed),
        Some("timeout") => Err(timeout_error()),
        Some("not_found") => Err(Error::NotFound("WebMCP tool unavailable".into())),
        Some("refused") => Err(Error::Refused(
            "WebMCP requires a non-secret tool and arguments".into(),
        )),
        Some("confirmation_required") => {
            let tool = &status["tool"];
            Err(Error::WebMcpConfirmation {
                tool: tool["name"].as_str().ok_or_else(failed)?.to_owned(),
                description: tool["description"].as_str().ok_or_else(failed)?.to_owned(),
                arguments: tool["arguments"].as_object().cloned().ok_or_else(failed)?,
            })
        }
        _ => Err(failed()),
    }
}

fn failed() -> Error {
    Error::Script("WebMCP operation failed".into())
}

impl Web {
    /// Discover tools afresh on the active page. An unavailable API is an empty list.
    pub async fn webmcp_list(&mut self) -> Result<Vec<WebMcpTool>> {
        let value = self
            .webmcp_operation(json!({"op": "list"}), WEBMCP_DEFAULT_TIMEOUT_MS)
            .await?;
        serde_json::from_value(value).map_err(|_| failed())
    }

    /// Call a page tool without a synthetic user gesture. `confirmed` is reserved
    /// for a trusted caller after real approval; it is not a page consent grant.
    /// Dropping this future aborts the in-flight call in the originating tab.
    pub async fn webmcp_call(
        &mut self,
        name: &str,
        input: Value,
        confirmed: bool,
        timeout_ms: u64,
    ) -> Result<Value> {
        if name.trim().is_empty() || !input.is_object() {
            return Err(Error::InvalidInput(
                "WebMCP needs a tool name and an argument object".into(),
            ));
        }
        if !(1..=WEBMCP_MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(Error::InvalidInput(
                "WebMCP timeout must be between 1 and 20000 ms".into(),
            ));
        }
        self.webmcp_operation(
            json!({"op": "call", "name": name, "input": input, "confirmed": confirmed}),
            timeout_ms,
        )
        .await
    }

    async fn webmcp_operation(&self, mut payload: Value, timeout_ms: u64) -> Result<Value> {
        let target = self.active.clone().ok_or_else(failed)?;
        let endpoint = self.cdp.endpoint().to_owned();
        let key = invocation_key()?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let (mut tx, rx) = oneshot::channel();
        // Use a short-lived attachment: the worker can deliver abort even if
        // the client future (and its borrow of Web) has already been dropped.
        tokio::spawn(async move {
            let setup = async {
                let mut cdp = Cdp::connect(&endpoint).await.map_err(|_| failed())?;
                let session = cdp.attach(&target).await.map_err(|_| failed())?;
                Ok::<_, Error>((cdp, session))
            };
            let (mut cdp, session) = tokio::select! {
                biased;
                _ = tx.closed() => return,
                result = timeout_at(deadline, setup) => match result {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(e)) => { let _ = tx.send(Err(e)); return; }
                    Err(_) => { let _ = tx.send(Err(timeout_error())); return; }
                }
            };
            payload["key"] = json!(&key);
            // Page timer supplies a real signal even if the CDP connection dies.
            payload["timeout"] = json!(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .max(1) as u64
            );
            let result = tokio::select! {
                biased;
                _ = tx.closed() => None,
                _ = tokio::time::sleep_until(deadline) => Some(Err(timeout_error())),
                result = cdp.call("Runtime.evaluate", evaluate(payload)) => {
                    let _ = tx.send(result.map_err(|_| failed()).and_then(decode));
                    return;
                }
            };
            // CDP commands are ordered on this socket. If the evaluation was
            // sent, its synchronous prologue installed the abort closure before
            // yielding. After navigation the key cannot target a new call.
            let _ = timeout_at(Instant::now() + Duration::from_secs(1), cdp.call_in(
                Some(&session), "Runtime.evaluate",
                json!({"expression": format!("globalThis[{}]?.()", json!(key)), "silent": true}),
                Duration::from_secs(1),
            )).await;
            if let Some(result) = result {
                let _ = tx.send(result);
            }
        });
        rx.await.map_err(|_| failed())?
    }
}
