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
const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_PAGE_BYTES: usize = 30 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
/// Default WebMCP deadline in milliseconds.
pub const WEBMCP_DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Maximum WebMCP deadline in milliseconds.
pub const WEBMCP_MAX_TIMEOUT_MS: u64 = 20_000;

/// Page-authored metadata; never an authorization grant or agent instructions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebMcpTool {
    /// Opaque invocation reference from this discovery, not the tool name.
    #[serde(rename = "ref", default)]
    pub tool_ref: String,
    /// The page-defined tool name.
    pub name: String,
    /// The page-defined description (empty for a secret-bearing tool).
    pub description: String,
    /// The page-defined JSON Schema, or null if absent or secret-bearing.
    pub input_schema: Value,
    /// Advisory page annotations and recognized safety markers.
    pub annotations: Value,
}

/// Discovery response. All entries in tools are untrusted page data.
#[derive(Debug, Serialize)]
pub struct WebMcpDiscovery {
    /// Always true, regardless of the page's annotations.
    pub untrusted: bool,
    /// Browser-reported origin of the document that supplied the tools.
    pub origin: Option<String>,
    /// Discovered registrations from the top-level document.
    pub tools: Vec<WebMcpTool>,
}

/// Result from a page tool, not a trusted tool message.
#[derive(Debug, Serialize)]
pub struct WebMcpResult {
    /// Always true, regardless of the page's annotations or result fields.
    pub untrusted: bool,
    /// Browser-reported origin of the document that supplied the result.
    pub origin: String,
    /// Untrusted page data. A page may report a domain error in this value.
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq)]
struct Document {
    frame: String,
    loader: String,
    origin: String,
}

#[derive(Debug)]
pub(super) struct Discovery {
    target: String,
    document: Document,
    realm: String,
    refs: Vec<String>,
    generation: String,
}

fn timeout_error() -> Error {
    Error::Timeout("WebMCP".into())
}

pub(super) fn random_key() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("wm_{hex}"))
}

fn evaluate(payload: Value, realm: &str) -> Value {
    // Parse arguments as JSON data, not an object literal (notably __proto__).
    let json_string = Value::String(payload.to_string());
    json!({
        "expression": format!("{BRIDGE}(JSON.parse({json_string}))"),
        "uniqueContextId": realm,
        "awaitPromise": true,
        "returnByValue": true,
        "silent": true,
    })
}

fn decode(reply: Value) -> Result<Value> {
    let wire = reply["result"]["value"].as_str().ok_or_else(failed)?;
    if wire.len() > MAX_PAGE_BYTES {
        return Err(too_large());
    }
    let status: Value = serde_json::from_str(wire).map_err(|_| failed())?;
    match status["status"].as_str() {
        Some("ok") => status.get("value").cloned().ok_or_else(failed),
        Some("timeout") => Err(timeout_error()),
        Some("stale") => Err(stale()),
        Some("too_large") => Err(too_large()),
        Some("not_found") => Err(Error::NotFound("WebMCP tool unavailable".into())),
        Some("refused") => Err(Error::Refused("WebMCP tool or arguments refused".into())),
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

fn stale() -> Error {
    Error::StaleRef("WebMCP discovery is no longer current; list tools again".into())
}

fn too_large() -> Error {
    Error::Refused("WebMCP response exceeds the size limit".into())
}

fn bounded_response<T: Serialize>(response: T) -> Result<T> {
    if serde_json::to_vec(&response).map_err(|_| failed())?.len() > MAX_RESPONSE_BYTES {
        return Err(too_large());
    }
    Ok(response)
}

async fn document(cdp: &mut Cdp) -> Result<Document> {
    let result = cdp.call("Page.getFrameTree", json!({})).await?;
    let frame = &result["frameTree"]["frame"];
    let field = |name: &str| frame[name].as_str().map(str::to_owned).ok_or_else(failed);
    Ok(Document {
        frame: field("id")?,
        loader: field("loaderId")?,
        origin: field("securityOrigin")?,
    })
}

async fn main_realm(cdp: &mut Cdp, doc: &Document) -> Result<String> {
    cdp.call("Runtime.enable", json!({})).await?;
    let realm = loop {
        let event = cdp
            .next_event(Duration::from_secs(1))
            .await?
            .ok_or_else(stale)?;
        let ctx = &event["params"]["context"];
        if event["method"] == "Runtime.executionContextCreated"
            && event["sessionId"].as_str() == cdp.session_id()
            && ctx["auxData"]["isDefault"] == true
            && ctx["auxData"]["frameId"] == doc.frame
        {
            break ctx["uniqueId"].as_str().ok_or_else(failed)?.to_owned();
        }
    };
    cdp.call("Runtime.disable", json!({})).await?;
    Ok(realm)
}

impl Web {
    /// Discover tools afresh. A new discovery invalidates the prior references.
    pub async fn webmcp_list(&mut self) -> Result<WebMcpDiscovery> {
        self.webmcp_discovery = None;
        let target = self.active.clone().ok_or_else(stale)?;
        let generation = random_key()?;
        let (value, doc, realm) = self
            .webmcp_operation(
                &target,
                None,
                json!({"op": "list", "generation": generation}),
                WEBMCP_DEFAULT_TIMEOUT_MS,
            )
            .await?;
        let mut tools: Vec<WebMcpTool> = serde_json::from_value(value).map_err(|_| failed())?;
        for (i, tool) in tools.iter_mut().enumerate() {
            tool.tool_ref = format!("{generation}_{i}");
        }
        let refs = tools.iter().map(|tool| tool.tool_ref.clone()).collect();
        let result = bounded_response(WebMcpDiscovery {
            untrusted: true,
            origin: (!tools.is_empty()).then(|| doc.origin.clone()),
            tools,
        })?;
        self.webmcp_discovery = Some(Discovery {
            target,
            document: doc,
            realm,
            refs,
            generation,
        });
        Ok(result)
    }

    /// Invoke a discovery ref, without a synthetic user gesture. Confirm only
    /// after independent approval. Dropping the future requests cancellation.
    pub async fn webmcp_call(
        &mut self,
        tool_ref: &str,
        input: Value,
        confirmed: bool,
        timeout_ms: u64,
    ) -> Result<WebMcpResult> {
        if !input.is_object()
            || serde_json::to_vec(&input).map_err(|_| failed())?.len() > MAX_INPUT_BYTES
        {
            return Err(Error::InvalidInput(
                "WebMCP needs a JSON argument object of at most 16 KiB".into(),
            ));
        }
        if !(1..=WEBMCP_MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(Error::InvalidInput(
                "WebMCP timeout must be between 1 and 20000 ms".into(),
            ));
        }
        let binding = self.webmcp_discovery.as_ref().ok_or_else(stale)?;
        if self.active.as_ref() != Some(&binding.target) {
            return Err(stale());
        }
        let index = binding
            .refs
            .iter()
            .position(|r| r == tool_ref)
            .ok_or_else(stale)?;
        let (value, doc, _) = self.webmcp_operation(
            &binding.target, Some(binding),
            json!({"op": "call", "index": index, "generation": binding.generation, "input": input, "confirmed": confirmed}),
            timeout_ms,
        ).await?;
        bounded_response(WebMcpResult {
            untrusted: true,
            origin: doc.origin,
            value,
        })
    }

    async fn webmcp_operation(
        &self,
        target: &str,
        binding: Option<&Discovery>,
        mut payload: Value,
        timeout_ms: u64,
    ) -> Result<(Value, Document, String)> {
        let target = target.to_owned();
        let endpoint = self.cdp.endpoint().to_owned();
        let expected = binding.map(|d| (d.document.clone(), d.realm.clone()));
        let key = random_key()?;
        let registry = &self.webmcp_registry;
        payload["registry"] = json!(registry);
        payload["key"] = json!(&key);
        payload["limit"] = json!(MAX_PAGE_BYTES);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let (mut tx, rx) = oneshot::channel();
        // A short-lived attachment lets us abort even after the caller drops
        // the future. Limit untrusted CDP messages on this socket only.
        tokio::spawn(async move {
            let setup = async {
                let mut cdp = Cdp::connect_bounded(&endpoint, Some(512 * 1024))
                    .await
                    .map_err(|_| failed())?;
                cdp.attach(&target).await.map_err(|_| stale())?;
                let doc = document(&mut cdp).await.map_err(|_| stale())?;
                let realm = match expected {
                    Some((expected, realm)) if expected == doc => realm,
                    Some(_) => return Err(stale()),
                    None => main_realm(&mut cdp, &doc).await.map_err(|_| stale())?,
                };
                Ok::<_, Error>((cdp, doc, realm))
            };
            let (mut cdp, doc, realm) = tokio::select! {
                biased;
                _ = tx.closed() => return,
                result = timeout_at(deadline, setup) => match result {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(e)) => { let _ = tx.send(Err(e)); return; }
                    Err(_) => { let _ = tx.send(Err(timeout_error())); return; }
                }
            };
            payload["timeout"] = json!(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .max(1) as u64
            );
            let run = async {
                let reply = cdp
                    .call("Runtime.evaluate", evaluate(payload, &realm))
                    .await;
                // Do not return content from a document that was replaced while
                // waiting. uniqueContextId also prevents dispatch to a successor.
                if document(&mut cdp).await.ok().as_ref() != Some(&doc) {
                    return Err(stale());
                }
                decode(reply.map_err(|_| failed())?)
            };
            let result = tokio::select! {
                biased;
                _ = tx.closed() => None,
                _ = tokio::time::sleep_until(deadline) => Some(Err(timeout_error())),
                result = run => {
                    let _ = tx.send(result.map(|value| (value, doc, realm)));
                    return;
                }
            };
            // Never send the abort closure lookup into a successor document.
            let _ = timeout_at(Instant::now() + Duration::from_secs(1), cdp.call(
                "Runtime.evaluate",
                json!({"expression": format!("globalThis[{}]?.()", json!(key)), "uniqueContextId": realm, "silent": true}),
            )).await;
            if let Some(result) = result {
                let _ = tx.send(result);
            }
        });
        rx.await.map_err(|_| failed())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_encoded_output_and_never_accepts_page_framing() {
        let value = json!({"untrusted":false,"origin":"https://other.example","value":"data"});
        let result =
            decode(json!({"result":{"value": json!({"status":"ok", "value":value}).to_string()}}))
                .unwrap();
        let framed = bounded_response(WebMcpResult {
            untrusted: true,
            origin: "https://example.com".into(),
            value: result,
        })
        .unwrap();
        let wire = json!(framed);
        assert_eq!(wire["untrusted"], true);
        assert_eq!(wire["origin"], "https://example.com");
        assert_eq!(wire["value"]["untrusted"], false);
        assert!(bounded_response("a".repeat(MAX_RESPONSE_BYTES - 2)).is_ok());
        assert!(bounded_response("\"".repeat(MAX_RESPONSE_BYTES / 2)).is_err());
        assert!(decode(json!({"result":{"value": "é".repeat(MAX_PAGE_BYTES / 2 + 1)}})).is_err());
    }
}
