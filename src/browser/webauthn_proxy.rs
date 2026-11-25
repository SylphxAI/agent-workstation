//! Origin-bound WebAuthn ceremony lease backed by Chromium's
//! `chrome.webAuthenticationProxy` extension API and an authenticated
//! host-side loopback signing bridge.
//!
//! Private keys never enter Chromium or its `FrameTreeNode`. Chromium's browser
//! process stamps `extensions.remoteDesktopClientOverride.{origin,
//! sameOriginWithAncestors}` on every intercepted ceremony request; the bridge
//! verifies extension authentication, single-use lease liveness, deadline,
//! exact canonical origin, and ancestor isolation before invoking the caller's
//! [`WebAuthnSigner`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cdp::Cdp;
use crate::gate::ControlGate;
use crate::webauthn::{
    WEBAUTHN_PROXY_EXTENSION_FLAG, WebAuthnCeremonyLeaseConfig, WebAuthnCeremonyOutcome,
    WebAuthnCeremonyScope, WebAuthnDenyReason, WebAuthnDomError, WebAuthnOperation,
    WebAuthnProxyResponse, WebAuthnSigner, validate_proxy_request,
};
use crate::{Error, Result};

const BRIDGE_JS_TEMPLATE: &str = include_str!("webauthn_proxy.js");
const TOKEN_HEADER: &str = "x-webauthn-bridge-token";
const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Optional session control gate watched while a ceremony lease is armed.
#[derive(Clone)]
pub(crate) struct ControlWatcher {
    gate: Arc<ControlGate>,
    principal: String,
    initial_hand_backs: u64,
}

impl ControlWatcher {
    pub(crate) fn new(gate: Arc<ControlGate>, principal: String) -> Self {
        let initial_hand_backs = gate.hand_backs();
        Self {
            gate,
            principal,
            initial_hand_backs,
        }
    }

    fn is_valid(&self) -> bool {
        !self.gate.is_closed()
            && self.gate.hand_backs() == self.initial_hand_backs
            && self.gate.check(&self.principal).is_ok()
    }
}

#[derive(Debug)]
enum Phase {
    Armed,
    InFlight,
    Terminal(WebAuthnCeremonyOutcome),
}

struct LeaseCore {
    allowed_origin: String,
    scope: WebAuthnCeremonyScope,
    require_same_origin_with_ancestors: bool,
    deadline: Instant,
    token: String,
    expected_extension_origin: OnceLock<String>,
    signer: Arc<dyn WebAuthnSigner>,
    control: Option<ControlWatcher>,
    phase: Mutex<Phase>,
    ready_tx: Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>>,
    detach_notify: Notify,
    terminal_notify: Notify,
    cancel_notify: Notify,
}

impl LeaseCore {
    fn current_outcome(&self) -> WebAuthnCeremonyOutcome {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*phase, Phase::Armed) && Instant::now() >= self.deadline {
            *phase = Phase::Terminal(WebAuthnCeremonyOutcome::Expired);
            self.terminal_notify.notify_waiters();
        }
        match &*phase {
            Phase::Armed | Phase::InFlight => WebAuthnCeremonyOutcome::Pending,
            Phase::Terminal(outcome) => outcome.clone(),
        }
    }

    fn cancel_if_active(&self, reason: WebAuthnCeremonyOutcome) {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        match *phase {
            Phase::Armed => {
                *phase = Phase::Terminal(reason);
                self.cancel_notify.notify_waiters();
                self.detach_notify.notify_waiters();
                self.terminal_notify.notify_waiters();
            }
            Phase::InFlight => {
                // Signal in-flight handler to abort and fail closed.
                self.cancel_notify.notify_waiters();
                self.detach_notify.notify_waiters();
            }
            Phase::Terminal(_) => {
                self.detach_notify.notify_waiters();
            }
        }
    }

    fn finish_with(&self, outcome: WebAuthnCeremonyOutcome) {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(*phase, Phase::Terminal(_)) {
            *phase = Phase::Terminal(outcome);
        }
        // Do not send a WebSocket detach before the HTTP decision reaches bg.js
        // and completeCreateRequest/completeGetRequest resolves; bg.js detaches
        // immediately after completing the ceremony.
        self.terminal_notify.notify_waiters();
    }

    fn is_cancelled_or_expired(&self) -> bool {
        if Instant::now() >= self.deadline {
            return true;
        }
        if self.control.as_ref().is_some_and(|c| !c.is_valid()) {
            return true;
        }
        matches!(
            *self.phase.lock().unwrap_or_else(|p| p.into_inner()),
            Phase::Terminal(_)
        )
    }
}

/// An armed, single-use, exact-origin WebAuthn ceremony lease.
///
/// While held, Chromium routes modal WebAuthn `create()` / `get()` requests
/// through an authenticated loopback bridge to the caller's out-of-browser
/// [`WebAuthnSigner`] after verifying Chromium's browser-process `origin` and
/// `sameOriginWithAncestors` assertions.
///
/// Call [`Self::finish`] or [`Self::cancel`] to detach the proxy and uninstall
/// the temporary extension deterministically. Dropping the lease without
/// awaiting teardown immediately closes the loopback bridge (causing the
/// extension to call `chrome.webAuthenticationProxy.detach()` fail-closed) and
/// schedules best-effort extension uninstall and temp-directory removal.
pub struct WebAuthnCeremonyLease {
    core: Arc<LeaseCore>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: Option<JoinHandle<()>>,
    watchdog_task: Option<JoinHandle<()>>,
    browser_cdp: Option<Cdp>,
    cdp_endpoint: String,
    extension_id: String,
    extension_dir: PathBuf,
    proxy_lease_active: Arc<AtomicBool>,
    disarmed: bool,
}

impl fmt::Debug for WebAuthnCeremonyLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebAuthnCeremonyLease")
            .field("allowed_origin", &self.core.allowed_origin)
            .field("outcome", &self.core.current_outcome())
            .field("disarmed", &self.disarmed)
            .finish_non_exhaustive()
    }
}

impl WebAuthnCeremonyLease {
    /// The exact canonical security origin bound to this lease.
    pub fn allowed_origin(&self) -> &str {
        &self.core.allowed_origin
    }

    /// Current outcome snapshot without detaching the lease.
    pub fn outcome(&self) -> WebAuthnCeremonyOutcome {
        self.core.current_outcome()
    }

    /// Complete the lease, detaching the proxy, uninstalling the temporary
    /// extension, removing its private directory, and returning the final
    /// [`WebAuthnCeremonyOutcome`].
    ///
    /// If a ceremony request is currently in flight, waits up to the remaining
    /// lease deadline for it to settle before tearing down.
    pub async fn finish(mut self) -> Result<WebAuthnCeremonyOutcome> {
        let is_in_flight = matches!(
            *self.core.phase.lock().unwrap_or_else(|p| p.into_inner()),
            Phase::InFlight
        );
        if is_in_flight {
            let _ = tokio::time::timeout_at(
                self.core.deadline + Duration::from_millis(250),
                self.core.terminal_notify.notified(),
            )
            .await;
        }
        self.teardown(WebAuthnCeremonyOutcome::Cancelled).await
    }

    /// Immediately revoke the lease fail-closed, detaching the proxy and
    /// uninstalling the temporary extension.
    pub async fn cancel(mut self) -> Result<WebAuthnCeremonyOutcome> {
        self.teardown(WebAuthnCeremonyOutcome::Cancelled).await
    }

    async fn teardown(
        &mut self,
        if_still_armed: WebAuthnCeremonyOutcome,
    ) -> Result<WebAuthnCeremonyOutcome> {
        if self.disarmed {
            return Ok(self.core.current_outcome());
        }
        self.disarmed = true;
        self.core.cancel_if_active(if_still_armed);

        if let Some(watchdog) = self.watchdog_task.take() {
            watchdog.abort();
        }
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        }

        let uninstall_result = uninstall_extension(
            self.browser_cdp.as_mut(),
            &self.cdp_endpoint,
            &self.extension_id,
        )
        .await;
        let _ = std::fs::remove_dir_all(&self.extension_dir);
        self.proxy_lease_active.store(false, Ordering::Release);

        let outcome = self.core.current_outcome();
        uninstall_result.map(|()| outcome)
    }
}

impl Drop for WebAuthnCeremonyLease {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.disarmed = true;
        self.core
            .cancel_if_active(WebAuthnCeremonyOutcome::Cancelled);
        if let Some(watchdog) = self.watchdog_task.take() {
            watchdog.abort();
        }
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server_task.take() {
            server.abort();
        }
        let cdp = self.browser_cdp.take();
        let endpoint = self.cdp_endpoint.clone();
        let ext_id = self.extension_id.clone();
        let dir = self.extension_dir.clone();
        let active = self.proxy_lease_active.clone();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut cdp = cdp;
                let _ = uninstall_extension(cdp.as_mut(), &endpoint, &ext_id).await;
                let _ = std::fs::remove_dir_all(&dir);
                active.store(false, Ordering::Release);
            });
        } else {
            let _ = std::fs::remove_dir_all(&dir);
            active.store(false, Ordering::Release);
        }
    }
}

pub(crate) async fn arm_lease(
    cdp_endpoint: String,
    proxy_lease_active: Arc<AtomicBool>,
    config: WebAuthnCeremonyLeaseConfig,
    signer: Arc<dyn WebAuthnSigner>,
    control: Option<ControlWatcher>,
) -> Result<WebAuthnCeremonyLease> {
    let allowed_origin = config.validate()?;
    if control.as_ref().is_some_and(|c| !c.is_valid()) {
        return Err(Error::Refused(
            "session control unavailable for WebAuthn ceremony lease".into(),
        ));
    }
    if proxy_lease_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::Refused(
            "an origin-bound WebAuthn ceremony lease is already active".into(),
        ));
    }

    let result = arm_lease_inner(
        cdp_endpoint,
        proxy_lease_active.clone(),
        allowed_origin,
        config,
        signer,
        control,
    )
    .await;
    if result.is_err() {
        proxy_lease_active.store(false, Ordering::Release);
    }
    result
}

async fn arm_lease_inner(
    cdp_endpoint: String,
    proxy_lease_active: Arc<AtomicBool>,
    allowed_origin: String,
    config: WebAuthnCeremonyLeaseConfig,
    signer: Arc<dyn WebAuthnSigner>,
    control: Option<ControlWatcher>,
) -> Result<WebAuthnCeremonyLease> {
    let token = random_hex(32)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let deadline = Instant::now() + config.timeout;

    let (ready_tx, ready_rx) = oneshot::channel();
    let core = Arc::new(LeaseCore {
        allowed_origin,
        scope: config.scope,
        require_same_origin_with_ancestors: config.require_same_origin_with_ancestors,
        deadline,
        token: token.clone(),
        expected_extension_origin: OnceLock::new(),
        signer,
        control: control.clone(),
        phase: Mutex::new(Phase::Armed),
        ready_tx: Mutex::new(Some(ready_tx)),
        detach_notify: Notify::new(),
        terminal_notify: Notify::new(),
        cancel_notify: Notify::new(),
    });

    let router = bridge_router(core.clone());
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    let extension_dir = write_extension_bundle(port, &token, config.report_uvpaa)?;

    let mut browser_cdp = match Cdp::connect(&cdp_endpoint).await {
        Ok(c) => c,
        Err(e) => {
            cleanup_failed_arm(
                shutdown_tx,
                server_task,
                &extension_dir,
                None,
                &cdp_endpoint,
            )
            .await;
            return Err(e);
        }
    };

    let loaded = browser_cdp
        .call_in(
            None,
            "Extensions.loadUnpacked",
            json!({
                "path": extension_dir.to_string_lossy(),
                "enableInIncognito": true,
            }),
            READY_TIMEOUT,
        )
        .await;
    let extension_id = match loaded {
        Ok(value) => match value["id"].as_str() {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => {
                cleanup_failed_arm(
                    shutdown_tx,
                    server_task,
                    &extension_dir,
                    None,
                    &cdp_endpoint,
                )
                .await;
                return Err(Error::protocol(
                    "Extensions.loadUnpacked returned no extension id",
                ));
            }
        },
        Err(err) => {
            cleanup_failed_arm(
                shutdown_tx,
                server_task,
                &extension_dir,
                None,
                &cdp_endpoint,
            )
            .await;
            let text = err.to_string();
            if text.contains("Method not available") || text.contains("wasn't found") {
                return Err(Error::Unsupported(format!(
                    "origin-bound WebAuthn ceremony lease requires Chromium launched with {WEBAUTHN_PROXY_EXTENSION_FLAG}"
                )));
            }
            return Err(err);
        }
    };

    let expected_origin = format!("chrome-extension://{extension_id}");
    let _ = core.expected_extension_origin.set(expected_origin.clone());

    let ready_wait = READY_TIMEOUT.min(config.timeout);
    let ready_origin = match tokio::time::timeout(ready_wait, ready_rx).await {
        Ok(Ok(Ok(origin))) => origin,
        Ok(Ok(Err(msg))) => {
            cleanup_failed_arm(
                shutdown_tx,
                server_task,
                &extension_dir,
                Some((&mut browser_cdp, &extension_id)),
                &cdp_endpoint,
            )
            .await;
            return Err(Error::protocol(format!(
                "WebAuthn proxy extension failed to attach: {msg}"
            )));
        }
        Ok(Err(_)) | Err(_) => {
            cleanup_failed_arm(
                shutdown_tx,
                server_task,
                &extension_dir,
                Some((&mut browser_cdp, &extension_id)),
                &cdp_endpoint,
            )
            .await;
            return Err(Error::Timeout("WebAuthn proxy extension attach".into()));
        }
    };

    if ready_origin != expected_origin {
        cleanup_failed_arm(
            shutdown_tx,
            server_task,
            &extension_dir,
            Some((&mut browser_cdp, &extension_id)),
            &cdp_endpoint,
        )
        .await;
        return Err(Error::Refused(
            "WebAuthn proxy bridge received unexpected extension origin".into(),
        ));
    }

    let watchdog_core = core.clone();
    let watchdog_endpoint = cdp_endpoint.clone();
    let watchdog_ext_id = extension_id.clone();
    let watchdog_task = tokio::spawn(async move {
        let expired = if let Some(watcher) = control {
            let mut rx = watcher.gate.subscribe();
            loop {
                if !watcher.is_valid() {
                    break false;
                }
                tokio::select! {
                    biased;
                    _ = watchdog_core.terminal_notify.notified() => return,
                    _ = tokio::time::sleep_until(watchdog_core.deadline) => break true,
                    changed = rx.changed() => {
                        if changed.is_err() || !watcher.is_valid() {
                            break false;
                        }
                    }
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = watchdog_core.terminal_notify.notified() => return,
                _ = tokio::time::sleep_until(watchdog_core.deadline) => true,
            }
        };
        let reason = if expired {
            WebAuthnCeremonyOutcome::Expired
        } else {
            WebAuthnCeremonyOutcome::Cancelled
        };
        watchdog_core.cancel_if_active(reason);
        let _ = uninstall_extension(None, &watchdog_endpoint, &watchdog_ext_id).await;
    });

    Ok(WebAuthnCeremonyLease {
        core,
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(server_task),
        watchdog_task: Some(watchdog_task),
        browser_cdp: Some(browser_cdp),
        cdp_endpoint,
        extension_id,
        extension_dir,
        proxy_lease_active,
        disarmed: false,
    })
}

async fn cleanup_failed_arm(
    shutdown_tx: oneshot::Sender<()>,
    server_task: JoinHandle<()>,
    extension_dir: &Path,
    installed: Option<(&mut Cdp, &str)>,
    cdp_endpoint: &str,
) {
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;
    if let Some((cdp, ext_id)) = installed {
        let _ = uninstall_extension(Some(cdp), cdp_endpoint, ext_id).await;
    }
    let _ = std::fs::remove_dir_all(extension_dir);
}

async fn uninstall_extension(
    existing_cdp: Option<&mut Cdp>,
    cdp_endpoint: &str,
    extension_id: &str,
) -> Result<()> {
    let call_result = if let Some(cdp) = existing_cdp {
        cdp.call_in(
            None,
            "Extensions.uninstall",
            json!({ "id": extension_id }),
            UNINSTALL_TIMEOUT,
        )
        .await
    } else {
        let Ok(mut cdp) = Cdp::connect(cdp_endpoint).await else {
            return Ok(());
        };
        cdp.call_in(
            None,
            "Extensions.uninstall",
            json!({ "id": extension_id }),
            UNINSTALL_TIMEOUT,
        )
        .await
    };
    match call_result {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("could not find extension") => Ok(()),
        Err(err) => Err(err),
    }
}

fn bridge_router(core: Arc<LeaseCore>) -> Router {
    Router::new()
        .route("/control", get(handle_control))
        .route("/request", post(handle_request))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(core)
}

fn valid_extension_origin(headers: &HeaderMap) -> Option<String> {
    let origin = headers.get("origin")?.to_str().ok()?;
    let rest = origin.strip_prefix("chrome-extension://")?;
    if rest.is_empty()
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return None;
    }
    Some(origin.to_owned())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlMessage {
    #[serde(rename = "type")]
    kind: String,
    token: String,
    message: Option<String>,
}

async fn handle_control(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(core): State<Arc<LeaseCore>>,
) -> Response {
    let Some(origin) = valid_extension_origin(&headers) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if let Some(expected) = core.expected_extension_origin.get()
        && origin != *expected
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| serve_control_socket(socket, origin, core))
}

async fn serve_control_socket(mut socket: WebSocket, origin: String, core: Arc<LeaseCore>) {
    let first = tokio::time::timeout(READY_TIMEOUT, socket.next()).await;
    let Ok(Some(Ok(Message::Text(text)))) = first else {
        let _ = socket.close().await;
        return;
    };
    let Ok(msg) = serde_json::from_str::<ControlMessage>(&text) else {
        let _ = socket.close().await;
        return;
    };
    if !constant_time_eq(msg.token.as_bytes(), core.token.as_bytes()) {
        let _ = socket.close().await;
        return;
    }
    let ready_tx = core
        .ready_tx
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(tx) = ready_tx {
        match msg.kind.as_str() {
            "ready" => {
                let _ = tx.send(Ok(origin));
            }
            _ => {
                let _ = tx.send(Err(msg
                    .message
                    .unwrap_or_else(|| "extension attach rejected".into())));
                let _ = socket.close().await;
                return;
            }
        }
    } else {
        // Only a single authenticated control connection is accepted per lease.
        let _ = socket.close().await;
        return;
    }

    tokio::select! {
        _ = core.detach_notify.notified() => {
            let _ = socket.send(Message::Text(r#"{"type":"detach"}"#.into())).await;
            let _ = socket.close().await;
        }
        _ = async {
            while let Some(Ok(frame)) = socket.next().await {
                if matches!(frame, Message::Close(_)) {
                    break;
                }
            }
        } => {}
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRequestPayload {
    kind: String,
    #[allow(dead_code)]
    request_id: i64,
    request_details_json: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeDecision {
    #[serde(skip_serializing_if = "Option::is_none")]
    response_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WebAuthnDomError>,
}

impl BridgeDecision {
    fn deny(message: impl Into<String>) -> axum::Json<Self> {
        axum::Json(Self {
            response_json: None,
            error: Some(WebAuthnDomError::not_allowed(message)),
        })
    }

    fn from_response(resp: WebAuthnProxyResponse) -> axum::Json<Self> {
        match resp {
            WebAuthnProxyResponse::Success { response_json } => axum::Json(Self {
                response_json: Some(response_json),
                error: None,
            }),
            WebAuthnProxyResponse::Error(err) => axum::Json(Self {
                response_json: None,
                error: Some(err),
            }),
        }
    }
}

async fn handle_request(
    headers: HeaderMap,
    State(core): State<Arc<LeaseCore>>,
    body: axum::body::Bytes,
) -> Response {
    // 1. Authenticate extension Origin and per-lease secret token before any state transition.
    let Some(expected_origin) = core.expected_extension_origin.get() else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(origin) = valid_extension_origin(&headers) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if origin != *expected_origin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token_hdr) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !constant_time_eq(token_hdr.as_bytes(), core.token.as_bytes()) {
        return StatusCode::FORBIDDEN.into_response();
    }

    // 2. Atomically claim the single-use ceremony slot, checking deadline and control gate.
    {
        let mut phase = core.phase.lock().unwrap_or_else(|p| p.into_inner());
        if Instant::now() >= core.deadline {
            if matches!(*phase, Phase::Armed) {
                *phase = Phase::Terminal(WebAuthnCeremonyOutcome::Expired);
                core.detach_notify.notify_waiters();
                core.terminal_notify.notify_waiters();
            }
            return BridgeDecision::deny("WebAuthn ceremony lease expired").into_response();
        }
        if core.control.as_ref().is_some_and(|c| !c.is_valid()) {
            if matches!(*phase, Phase::Armed) {
                *phase = Phase::Terminal(WebAuthnCeremonyOutcome::Cancelled);
                core.detach_notify.notify_waiters();
                core.terminal_notify.notify_waiters();
            }
            return BridgeDecision::deny("WebAuthn ceremony lease revoked by control change")
                .into_response();
        }
        match *phase {
            Phase::Armed => *phase = Phase::InFlight,
            Phase::InFlight | Phase::Terminal(_) => {
                return BridgeDecision::deny("WebAuthn ceremony lease already consumed")
                    .into_response();
            }
        }
    }

    // 3. Parse and enforce browser-process origin + ancestor + scope invariants BEFORE signing.
    let Ok(payload) = serde_json::from_slice::<BridgeRequestPayload>(&body) else {
        core.finish_with(WebAuthnCeremonyOutcome::Denied {
            reason: WebAuthnDenyReason::MalformedRequest,
        });
        return BridgeDecision::deny("Malformed WebAuthn bridge request").into_response();
    };
    let operation = match payload.kind.as_str() {
        "create" => WebAuthnOperation::Create,
        "get" => WebAuthnOperation::Get,
        _ => {
            core.finish_with(WebAuthnCeremonyOutcome::Denied {
                reason: WebAuthnDenyReason::MalformedRequest,
            });
            return BridgeDecision::deny("Unsupported WebAuthn operation").into_response();
        }
    };
    let verified = match validate_proxy_request(
        operation,
        payload.request_details_json,
        &core.allowed_origin,
        core.scope,
        core.require_same_origin_with_ancestors,
    ) {
        Ok(req) => req,
        Err(reason) => {
            core.finish_with(WebAuthnCeremonyOutcome::Denied { reason });
            return BridgeDecision::deny(deny_message(reason)).into_response();
        }
    };

    if core.is_cancelled_or_expired() {
        let outcome = if Instant::now() >= core.deadline {
            WebAuthnCeremonyOutcome::Expired
        } else {
            WebAuthnCeremonyOutcome::Cancelled
        };
        core.finish_with(outcome);
        return BridgeDecision::deny("WebAuthn ceremony lease no longer active").into_response();
    }

    // 4. Invoke caller's out-of-browser host signer, bounded by remaining deadline and cancellation.
    let verified_origin = verified.origin.clone();
    let verified_same_origin = verified.same_origin_with_ancestors;
    let decision = tokio::select! {
        biased;
        _ = core.cancel_notify.notified() => None,
        _ = tokio::time::sleep_until(core.deadline) => None,
        response = core.signer.handle(verified) => Some(response),
    };

    // Re-check authority right before delivering a signed response to Chromium.
    let Some(response) = decision else {
        let outcome = if Instant::now() >= core.deadline {
            WebAuthnCeremonyOutcome::Expired
        } else {
            WebAuthnCeremonyOutcome::Cancelled
        };
        core.finish_with(outcome);
        return BridgeDecision::deny("WebAuthn ceremony lease expired or cancelled")
            .into_response();
    };
    if core.is_cancelled_or_expired() {
        let outcome = if Instant::now() >= core.deadline {
            WebAuthnCeremonyOutcome::Expired
        } else {
            WebAuthnCeremonyOutcome::Cancelled
        };
        core.finish_with(outcome);
        return BridgeDecision::deny("WebAuthn ceremony lease expired or cancelled")
            .into_response();
    }

    let outcome = match &response {
        WebAuthnProxyResponse::Success { .. } => WebAuthnCeremonyOutcome::Completed {
            operation,
            origin: verified_origin,
            same_origin_with_ancestors: verified_same_origin,
        },
        WebAuthnProxyResponse::Error(err) => WebAuthnCeremonyOutcome::SignerError {
            operation,
            origin: verified_origin,
            error_name: err.name.clone(),
        },
    };
    core.finish_with(outcome);
    BridgeDecision::from_response(response).into_response()
}

fn deny_message(reason: WebAuthnDenyReason) -> &'static str {
    match reason {
        WebAuthnDenyReason::OriginMismatch => {
            "Denied: calling origin does not match ceremony lease"
        }
        WebAuthnDenyReason::CrossOriginAncestor => {
            "Denied: cross-origin ancestor frame is not allowed by ceremony lease"
        }
        WebAuthnDenyReason::MissingOriginMetadata => {
            "Denied: missing browser-verified origin metadata"
        }
        WebAuthnDenyReason::ScopeMismatch => "Denied: WebAuthn operation outside lease scope",
        WebAuthnDenyReason::MalformedRequest => "Denied: malformed WebAuthn ceremony request",
        WebAuthnDenyReason::Expired => "Denied: WebAuthn ceremony lease expired",
        WebAuthnDenyReason::AlreadyUsed => "Denied: WebAuthn ceremony lease already consumed",
        WebAuthnDenyReason::Cancelled => "Denied: WebAuthn ceremony lease cancelled",
    }
}

fn random_hex(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_extension_bundle(port: u16, token: &str, report_uvpaa: bool) -> Result<PathBuf> {
    let nonce = random_hex(16)?;
    let dir = std::env::temp_dir().join(format!("agent-workstation-webauthn-proxy-{nonce}"));
    create_private_dir(&dir)?;

    let manifest = json!({
        "manifest_version": 3,
        "name": "WebAuthn Origin-Bound Ceremony Bridge",
        "version": "1.0",
        "incognito": "spanning",
        "permissions": ["webAuthenticationProxy"],
        "host_permissions": [format!("http://127.0.0.1:{port}/*")],
        "background": { "service_worker": "bg.js" }
    });
    let config = json!({
        "wsUrl": format!("ws://127.0.0.1:{port}/control"),
        "requestUrl": format!("http://127.0.0.1:{port}/request"),
        "token": token,
        "reportUvpaa": report_uvpaa,
    });
    let bg_js = format!("const CONFIG = {};\n{BRIDGE_JS_TEMPLATE}", config);

    if let Err(err) = (|| -> std::io::Result<()> {
        write_private_file(&dir.join("manifest.json"), manifest.to_string().as_bytes())?;
        write_private_file(&dir.join("bg.js"), bg_js.as_bytes())?;
        Ok(())
    })() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err.into());
    }
    Ok(dir)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::webauthn::webauthn_signer;

    fn test_core(
        allowed_origin: &str,
        scope: WebAuthnCeremonyScope,
        require_same_origin: bool,
        timeout: Duration,
        calls: Arc<AtomicUsize>,
    ) -> Arc<LeaseCore> {
        let core = Arc::new(LeaseCore {
            allowed_origin: allowed_origin.into(),
            scope,
            require_same_origin_with_ancestors: require_same_origin,
            deadline: Instant::now() + timeout,
            token: "0123456789abcdef0123456789abcdef".into(),
            expected_extension_origin: OnceLock::new(),
            signer: Arc::new(webauthn_signer(move |_req| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    WebAuthnProxyResponse::Success {
                        response_json: r#"{"type":"public-key"}"#.into(),
                    }
                }
            })),
            control: None,
            phase: Mutex::new(Phase::Armed),
            ready_tx: Mutex::new(None),
            detach_notify: Notify::new(),
            terminal_notify: Notify::new(),
            cancel_notify: Notify::new(),
        });
        let _ = core
            .expected_extension_origin
            .set("chrome-extension://abcdefghijklmnopabcdefghijklmnop".into());
        core
    }

    fn request_body(origin: &str, same_origin: bool) -> String {
        json!({
            "kind": "get",
            "requestId": 1,
            "requestDetailsJson": json!({
                "rpId": "example.com",
                "challenge": "AAAA",
                "extensions": {
                    "remoteDesktopClientOverride": {
                        "origin": origin,
                        "sameOriginWithAncestors": same_origin
                    }
                }
            }).to_string()
        })
        .to_string()
    }

    #[tokio::test]
    async fn bridge_refuses_unauthenticated_and_wrong_origin_requests_with_zero_signer_calls() {
        let calls = Arc::new(AtomicUsize::new(0));
        let core = test_core(
            "https://app.example.com",
            WebAuthnCeremonyScope::Get,
            true,
            Duration::from_secs(10),
            calls.clone(),
        );
        let app = bridge_router(core.clone());

        // Missing token
        let resp = app
            .clone()
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.headers().get("access-control-allow-origin").is_none());

        // Wrong token
        let resp = app
            .clone()
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    )
                    .header(TOKEN_HEADER, "wrong-token")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Web-page Origin header (even with valid token!)
        let resp = app
            .clone()
            .oneshot(
                Request::post("/request")
                    .header("origin", "https://app.example.com")
                    .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Different extension ID Origin header (even with valid token!)
        let resp = app
            .clone()
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                    )
                    .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Unauthenticated probes did not consume the lease; valid extension request succeeds once.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    )
                    .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            core.current_outcome(),
            WebAuthnCeremonyOutcome::Completed {
                operation: WebAuthnOperation::Get,
                origin: "https://app.example.com".into(),
                same_origin_with_ancestors: true,
            }
        );

        // Second authenticated request on the single-use lease is denied with zero extra signer calls.
        let resp = app
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    )
                    .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["name"], "NotAllowedError");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bridge_denies_origin_mismatch_and_cross_origin_iframe_before_signer() {
        for (req_origin, same_origin, expected_reason) in [
            (
                "https://other.example.com",
                true,
                WebAuthnDenyReason::OriginMismatch,
            ),
            (
                "https://app.example.com",
                false,
                WebAuthnDenyReason::CrossOriginAncestor,
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let core = test_core(
                "https://app.example.com",
                WebAuthnCeremonyScope::Any,
                true,
                Duration::from_secs(10),
                calls.clone(),
            );
            let app = bridge_router(core.clone());
            let resp = app
                .oneshot(
                    Request::post("/request")
                        .header(
                            "origin",
                            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                        )
                        .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                        .header("content-type", "application/json")
                        .body(Body::from(request_body(req_origin, same_origin)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["name"], "NotAllowedError");
            assert!(body.get("responseJson").is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                core.current_outcome(),
                WebAuthnCeremonyOutcome::Denied {
                    reason: expected_reason,
                }
            );
        }
    }

    #[tokio::test]
    async fn bridge_revokes_on_human_takeover_including_intervening_hand_back() {
        let gate = ControlGate::new();
        gate.acquire("agent-a").unwrap();
        let watcher = ControlWatcher::new(gate.clone(), "agent-a".into());
        assert!(watcher.is_valid());

        // Even if a person takes control and immediately hands back, an earlier
        // ceremony lease must not survive across an intervening takeover cycle.
        gate.take(Some("human".into()));
        assert!(!watcher.is_valid());
        gate.release(None);
        assert!(!watcher.is_valid());

        let calls = Arc::new(AtomicUsize::new(0));
        let core = Arc::new(LeaseCore {
            allowed_origin: "https://app.example.com".into(),
            scope: WebAuthnCeremonyScope::Get,
            require_same_origin_with_ancestors: true,
            deadline: Instant::now() + Duration::from_secs(10),
            token: "0123456789abcdef0123456789abcdef".into(),
            expected_extension_origin: OnceLock::new(),
            signer: Arc::new(webauthn_signer({
                let calls = calls.clone();
                move |_req| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        WebAuthnProxyResponse::Success {
                            response_json: r#"{"type":"public-key"}"#.into(),
                        }
                    }
                }
            })),
            control: Some(watcher),
            phase: Mutex::new(Phase::Armed),
            ready_tx: Mutex::new(None),
            detach_notify: Notify::new(),
            terminal_notify: Notify::new(),
            cancel_notify: Notify::new(),
        });
        let _ = core
            .expected_extension_origin
            .set("chrome-extension://abcdefghijklmnopabcdefghijklmnop".into());

        let app = bridge_router(core.clone());
        let resp = app
            .oneshot(
                Request::post("/request")
                    .header(
                        "origin",
                        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    )
                    .header(TOKEN_HEADER, "0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body("https://app.example.com", true)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["name"], "NotAllowedError");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(core.current_outcome(), WebAuthnCeremonyOutcome::Cancelled);
    }

    #[tokio::test]
    async fn arm_lease_maps_unavailable_extensions_domain_to_unsupported_and_clears_lock() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(req["method"], "Extensions.loadUnpacked");
                let reply = json!({
                    "id": req["id"],
                    "error": { "code": -32000, "message": "Method not available." }
                });
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    reply.to_string().into(),
                ))
                .await
                .unwrap();
            }
        });

        let active = Arc::new(AtomicBool::new(false));
        let err = arm_lease(
            format!("ws://{addr}"),
            active.clone(),
            WebAuthnCeremonyLeaseConfig::new("https://app.example.com"),
            Arc::new(webauthn_signer(|_| async {
                WebAuthnProxyResponse::Error(WebAuthnDomError::not_allowed("unused"))
            })),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "unexpected: {err}");
        assert!(!active.load(Ordering::Acquire));
        server.await.unwrap();
    }
}
