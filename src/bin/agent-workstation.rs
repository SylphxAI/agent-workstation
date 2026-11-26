//! Standalone daemon and compact CLI for `agent-workstation`.
//!
//! Daemon mode (legacy top-level flags or `agent-workstation daemon ...`):
//! ```text
//! agent-workstation --listen 127.0.0.1:7600 --state-dir /var/lib/agent-workstation \
//!     --browser /usr/bin/chromium [--seed-flag --fingerprint] [--browser-arg ARG]... \
//!     [--secret-command CMD [ARG...] --]
//! ```
//!
//! Compact CLI mode (drives a running daemon):
//! ```text
//! agent-workstation open https://example.com
//! agent-workstation snapshot
//! agent-workstation click @e1
//! agent-workstation fill @e2 "hello"
//! agent-workstation fill-secret @e3 vault/item/password --expect-origin https://example.com
//! agent-workstation eval "document.title"
//! agent-workstation screenshot ./page.png
//! agent-workstation desktop snapshot
//! agent-workstation takeover start
//! agent-workstation close
//! agent-workstation skills [core|desktop|takeover]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_workstation::cli::{
    Cli, ClientArgs, Command, DaemonArgs, DesktopClickTarget, DesktopCommand, TakeoverCommand,
    parse_desktop_click_target,
};
use agent_workstation::{
    BrowserConfig, Button, Cookie, DesktopAction, DesktopSession, Error, Holder, Input, NodeRef,
    Profile, Proxy, Secret, SecretResolver, SessionConfig, TreeOptions, WEBMCP_DEFAULT_TIMEOUT_MS,
    native_adapter,
};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use clap::Parser;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes as HyperBytes;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::Notify;

#[derive(Debug)]
struct Options {
    listen: std::net::SocketAddr,
    state_dir: PathBuf,
    browser: BrowserConfig,
    secret_command: Option<Vec<String>>,
}

impl Options {
    fn from_daemon_args(args: DaemonArgs) -> Result<Self, String> {
        let browser = args
            .browser
            .ok_or_else(|| "--browser is required".to_owned())?;
        let state_dir = args
            .state_dir
            .ok_or_else(|| "--state-dir is required".to_owned())?;
        let mut cfg = BrowserConfig::new(browser);
        cfg.seed_flag = args.seed_flag;
        cfg.extra_args = args.browser_args;
        Ok(Self {
            listen: args.listen,
            state_dir,
            browser: cfg,
            secret_command: args.secret_command,
        })
    }
}

#[derive(Debug)]
struct CommandResolver(Vec<String>);

#[async_trait::async_trait]
impl SecretResolver for CommandResolver {
    async fn resolve(&self, secret_ref: &str) -> agent_workstation::Result<Secret> {
        let out = tokio::process::Command::new(&self.0[0])
            .args(&self.0[1..])
            .arg(secret_ref)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await?;
        if !out.status.success() {
            return Err(Error::Refused(format!(
                "secret command failed for {secret_ref:?}"
            )));
        }
        let mut v = String::from_utf8(out.stdout)
            .map_err(|_| Error::Refused("secret is not UTF-8".into()))?;
        if v.ends_with('\n') {
            v.pop();
        }
        Ok(Secret::new(v))
    }
}

struct App {
    token: String,
    opts: Options,
    resolver: Option<CommandResolver>,
    registry: Mutex<Registry>,
    changed: Notify,
}

type Shared = Arc<App>;

// A single short-held lock serializes admission with both kinds of fence.
// No lock is held across I/O or an await.
#[derive(Debug, Default)]
struct Fence {
    quiesced: bool,
    generation: u64,
    in_flight: usize,
}

struct ManagedSession {
    session: Arc<DesktopSession>,
    fence: Fence,
}

#[derive(Default)]
struct Registry {
    daemon: Fence,
    sessions: HashMap<String, ManagedSession>,
}

struct InFlight {
    app: Shared,
    id: Option<String>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut registry = self.app.registry.lock().expect("registry lock poisoned");
        registry.daemon.in_flight -= 1;
        if let Some(entry) = self
            .id
            .as_ref()
            .and_then(|id| registry.sessions.get_mut(id))
        {
            entry.fence.in_flight -= 1;
        }
        drop(registry);
        self.app.changed.notify_waiters();
    }
}

fn conflict(code: &str, message: &str) -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        json!({ "code": code, "message": message }),
    )
}

fn quiesced() -> ApiError {
    conflict("quiesced", "session is quiesced for snapshot")
}

impl App {
    fn admit(
        self: &Shared,
        id: Option<String>,
    ) -> Result<(InFlight, Option<Arc<DesktopSession>>), ApiError> {
        let mut registry = self.registry.lock().expect("registry lock poisoned");
        if registry.daemon.quiesced {
            return Err(quiesced());
        }
        let session = if let Some(id) = &id {
            let entry = registry
                .sessions
                .get_mut(id)
                .ok_or_else(|| ApiError::from(Error::NotFound(id.clone())))?;
            if entry.fence.quiesced {
                return Err(quiesced());
            }
            entry.fence.in_flight += 1;
            Some(Arc::clone(&entry.session))
        } else {
            None
        };
        registry.daemon.in_flight += 1;
        Ok((
            InFlight {
                app: Arc::clone(self),
                id,
            },
            session,
        ))
    }
}

// Authentication and admission run before body extraction. In particular, a
// stalled upload is already counted and cannot slip past a snapshot fence.
async fn authenticate(
    State(app): State<Shared>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    authorize(&app, request.headers())?;
    Ok(next.run(request).await)
}

async fn supervise(
    State((app, needs_session)): State<(Shared, bool)>,
    path: Result<AxumPath<String>, PathRejection>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let id = if needs_session {
        Some(path.map_err(|e| bad(e.to_string()))?.0)
    } else {
        None
    };
    let (_guard, session) = app.admit(id)?;
    if let Some(session) = session {
        request.extensions_mut().insert(session);
    }
    Ok(next.run(request).await)
}

async fn health(State(app): State<Shared>) -> Json<Value> {
    let registry = app.registry.lock().expect("registry lock poisoned");
    Json(
        json!({ "status": "ok", "sessions": registry.sessions.len(), "quiesced": registry.daemon.quiesced }),
    )
}

async fn sessions(State(app): State<Shared>) -> Json<Value> {
    let registry = app.registry.lock().expect("registry lock poisoned");
    let mut entries: Vec<_> = registry.sessions.iter().map(|(id, entry)| json!({
        "id": id,
        "takeover_url": entry.session.takeover_url().map(|url| url.to_string()).unwrap_or_default(),
        "quiesced": registry.daemon.quiesced || entry.fence.quiesced,
        "in_flight": entry.fence.in_flight,
    })).collect();
    entries.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(json!({ "sessions": entries }))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QuiesceRequest {
    timeout_ms: u64,
}

impl Default for QuiesceRequest {
    fn default() -> Self {
        Self { timeout_ms: 250 }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeRequest {}

fn optional_body<T: serde::de::DeserializeOwned + Default>(body: &Bytes) -> Result<T, ApiError> {
    if body.is_empty() {
        Ok(T::default())
    } else {
        serde_json::from_slice(body).map_err(|e| bad(e.to_string()))
    }
}

fn fence_mut<'a>(registry: &'a mut Registry, id: Option<&str>) -> Result<&'a mut Fence, ApiError> {
    match id {
        None => Ok(&mut registry.daemon),
        Some(id) => registry
            .sessions
            .get_mut(id)
            .map(|entry| &mut entry.fence)
            .ok_or_else(|| ApiError::from(Error::NotFound(id.to_owned()))),
    }
}

const CONTROL_BUDGET: Duration = Duration::from_millis(250);
const CONTROL_BODY_LIMIT: usize = 64 * 1024;

async fn control_body(body: Body, started: Instant) -> Result<Bytes, ApiError> {
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(started + CONTROL_BUDGET),
        to_bytes(body, CONTROL_BODY_LIMIT),
    )
    .await
    .map_err(|_| ApiError(
        StatusCode::REQUEST_TIMEOUT,
        json!({ "code": "control_body_timeout", "message": "control body did not arrive within budget" }),
    ))?
    .map_err(|e| bad(e.to_string()))
}

fn drain_timeout() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({ "code": "quiesce_timeout", "message": "in-flight calls did not drain within budget" }),
    )
}

async fn drain(
    app: &App,
    id: Option<&str>,
    body: Body,
    started: Instant,
) -> Result<Json<Value>, ApiError> {
    let body = control_body(body, started).await?;
    let req: QuiesceRequest = optional_body(&body)?;
    let deadline = started + Duration::from_millis(req.timeout_ms.clamp(1, 250));
    let generation = {
        let mut registry = app.registry.lock().expect("registry lock poisoned");
        let fence = fence_mut(&mut registry, id)?;
        fence.quiesced = true;
        fence.generation
    };
    let wait = async {
        loop {
            if Instant::now() >= deadline {
                return Err(drain_timeout());
            }
            // Register before testing the predicate: dropping the last call
            // between the predicate and await must not lose its notification.
            let changed = app.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut registry = app.registry.lock().expect("registry lock poisoned");
                let fence = fence_mut(&mut registry, id)?;
                if !fence.quiesced || fence.generation != generation {
                    return Err(conflict(
                        "quiesce_cancelled",
                        "quiesce was interrupted by resume",
                    ));
                }
                if fence.in_flight == 0 {
                    return Ok(Json(
                        json!({ "quiesced": true, "drained_ms": started.elapsed().as_millis() as u64 }),
                    ));
                }
            }
            changed.await;
        }
    };
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), wait)
        .await
        .unwrap_or_else(|_| Err(drain_timeout()))
}

async fn quiesce_all(State(app): State<Shared>, body: Body) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    drain(&app, None, body, started).await
}

async fn quiesce_session(
    State(app): State<Shared>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    drain(&app, Some(&id), body, started).await
}

async fn clear(
    app: &App,
    id: Option<&str>,
    body: Body,
    started: Instant,
) -> Result<Json<Value>, ApiError> {
    let body = control_body(body, started).await?;
    let _: ResumeRequest = optional_body(&body)?;
    {
        let mut registry = app.registry.lock().expect("registry lock poisoned");
        let fence = fence_mut(&mut registry, id)?;
        fence.quiesced = false;
        fence.generation += 1;
    }
    app.changed.notify_waiters();
    Ok(Json(json!({ "quiesced": false })))
}

async fn resume_all(State(app): State<Shared>, body: Body) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    clear(&app, None, body, started).await
}

async fn resume_session(
    State(app): State<Shared>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    clear(&app, Some(&id), body, started).await
}

struct ApiError(StatusCode, Value);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        if let Error::WebMcpConfirmation {
            tool,
            description,
            arguments,
        } = &e
        {
            return ApiError(
                StatusCode::FORBIDDEN,
                json!({
                    "code": e.code(), "message": e.to_string(), "tool": tool,
                    "description": description, "arguments": arguments, "untrusted": true,
                }),
            );
        }
        let status = match &e {
            Error::HumanInControl { .. } | Error::Leased { .. } => StatusCode::CONFLICT,
            Error::StaleRef(_) => StatusCode::CONFLICT,
            Error::Closed => StatusCode::GONE,
            Error::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Refused(_) => StatusCode::FORBIDDEN,
            Error::Unsupported(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::Starting { .. } | Error::Timeout(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(
            status,
            json!({ "code": e.code(), "message": e.to_string() }),
        )
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        json!({ "code": "invalid-input", "message": msg.into() }),
    )
}

fn authorize(app: &App, headers: &HeaderMap) -> Result<(), ApiError> {
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = given.is_some_and(|g| {
        g.len() == app.token.len()
            && g.bytes()
                .zip(app.token.bytes())
                .fold(0u8, |a, (x, y)| a | (x ^ y))
                == 0
    });
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            json!({ "code": "unauthorized" }),
        ))
    }
}

#[derive(Deserialize)]
struct CreateSession {
    profile: String,
    profile_dir: PathBuf,
    proxy: Option<String>,
}

async fn create(
    State(app): State<Shared>,
    Json(req): Json<CreateSession>,
) -> Result<Json<Value>, ApiError> {
    if req.profile.is_empty()
        || !req
            .profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(bad("profile must be [A-Za-z0-9._-]+"));
    }
    let id = random_id()?;
    let mut cfg = SessionConfig::new(
        Profile::new(req.profile, req.profile_dir),
        app.opts.browser.clone(),
        app.opts.state_dir.join(&id),
    );
    cfg.proxy = req.proxy.as_deref().map(Proxy::parse).transpose()?;
    let session = DesktopSession::start(cfg).await?;
    let url = session.takeover_url().map(|u| u.to_string());
    app.registry
        .lock()
        .expect("registry lock poisoned")
        .sessions
        .insert(
            id.clone(),
            ManagedSession {
                session: Arc::new(session),
                fence: Fence::default(),
            },
        );
    Ok(Json(json!({ "id": id, "takeover_url": url })))
}

fn parse_if_agent(uri: &Uri) -> Result<bool, ApiError> {
    let Some(query) = uri.query() else {
        return Ok(false);
    };
    let mut if_agent = false;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if k == "if_agent" {
            match v.as_ref() {
                "true" | "1" => if_agent = true,
                "false" | "0" => if_agent = false,
                other => {
                    return Err(bad(format!(
                        "invalid if_agent query value {other:?}; expected true or false"
                    )));
                }
            }
        }
    }
    Ok(if_agent)
}

async fn remove(
    State(app): State<Shared>,
    Extension(session): Extension<Arc<DesktopSession>>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    if parse_if_agent(&uri)? {
        if !session.close_if_agent().await {
            if let Holder::Human { since, .. } = session.gate().holder() {
                return Err(ApiError::from(Error::HumanInControl { since }));
            }
            app.registry
                .lock()
                .expect("registry lock poisoned")
                .sessions
                .remove(&id);
            return Err(ApiError::from(Error::Closed));
        }
    } else {
        session.close().await;
    }
    app.registry
        .lock()
        .expect("registry lock poisoned")
        .sessions
        .remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn takeover_status(
    State(app): State<Shared>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .registry
        .lock()
        .expect("registry lock poisoned")
        .sessions
        .get(&id)
        .map(|entry| Arc::clone(&entry.session))
        .ok_or_else(|| ApiError::from(Error::NotFound(id)))?;
    let value = serde_json::to_value(session.takeover_status()).map_err(|e| bad(e.to_string()))?;
    Ok(Json(value))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeoverStartRequest {
    principal: Option<String>,
}

async fn takeover_start(
    Extension(session): Extension<Arc<DesktopSession>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let req: TakeoverStartRequest = optional_body(&body)?;
    session.gate().take(req.principal);
    let value = serde_json::to_value(session.takeover_status()).map_err(|e| bad(e.to_string()))?;
    Ok(Json(value))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeoverEndRequest {
    note: Option<String>,
}

async fn takeover_end(
    Extension(session): Extension<Arc<DesktopSession>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let req: TakeoverEndRequest = optional_body(&body)?;
    let note = req
        .note
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty());
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > agent_workstation::takeover::MAX_NOTE_CHARS)
    {
        return Err(bad(format!(
            "note exceeds {} characters",
            agent_workstation::takeover::MAX_NOTE_CHARS
        )));
    }
    let released = session.gate().release(note);
    let status = session.takeover_status();
    Ok(Json(json!({
        "released": released,
        "status": status,
    })))
}

#[derive(Deserialize)]
struct Call {
    principal: String,
    #[serde(flatten)]
    op: Op,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    CurrentUrl,
    Tabs,
    NewTab {
        url: String,
    },
    SelectTab {
        id: String,
    },
    CloseTab {
        id: String,
    },
    Snapshot,
    Read,
    Click {
        r#ref: String,
    },
    Fill {
        r#ref: String,
        text: String,
    },
    FillSecret {
        r#ref: String,
        secret_ref: String,
        expect_origin: Option<String>,
    },
    Focus {
        r#ref: String,
    },
    Press {
        key: String,
        hold_ms: Option<u64>,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    ReleaseKeys,
    Eval {
        expression: String,
    },
    WebmcpList,
    WebmcpCall {
        #[serde(rename = "ref")]
        tool_ref: String,
        #[serde(default = "webmcp_empty_arguments")]
        arguments: Value,
        #[serde(default)]
        confirmed: bool,
        #[serde(default = "webmcp_default_timeout")]
        timeout_ms: u64,
    },
    Html {
        r#ref: Option<String>,
    },
    Cookies {
        #[serde(default)]
        urls: Vec<String>,
    },
    SetCookies {
        cookies: Vec<Cookie>,
    },
    DeleteCookies {
        name: String,
        url: Option<String>,
    },
    ClearCookies,
    PageScreenshot {
        #[serde(default)]
        full_page: bool,
    },
    Screenshot,
    ClickAt {
        x: f64,
        y: f64,
        button: Option<Button>,
        modifiers: Option<String>,
    },
    DesktopWindows,
    DesktopSnapshot {
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
        pid: Option<u32>,
        root: Option<String>,
        offset: Option<usize>,
    },
    DesktopSetValue {
        r#ref: String,
        value: String,
    },
    DesktopCapabilities,
    DesktopFocus {
        target: String,
    },
    DesktopClickRef {
        r#ref: String,
        button: Option<Button>,
        modifiers: Option<String>,
    },
    DesktopDoubleClickRef {
        r#ref: String,
    },
    Key {
        chord: String,
    },
    HoldKey {
        chord: String,
        ms: u64,
    },
    Type {
        text: String,
    },
    Scroll {
        x: f64,
        y: f64,
        dx: i32,
        dy: i32,
    },
    PointerPosition,
    ClipboardGet,
    ClipboardSet {
        text: String,
    },
    HandBack,
    Release,
}

fn webmcp_empty_arguments() -> Value {
    json!({})
}

fn webmcp_default_timeout() -> u64 {
    WEBMCP_DEFAULT_TIMEOUT_MS
}

async fn call(
    State(app): State<Shared>,
    Extension(session): Extension<Arc<DesktopSession>>,
    Json(Call { principal, op }): Json<Call>,
) -> Result<Json<Value>, ApiError> {
    let s = session.lease(&principal)?;
    let png = |bytes: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(bytes);
    let v = match op {
        Op::Navigate { url } => json!({ "url": s.navigate(&url).await? }),
        Op::Back => json!({ "url": s.back().await? }),
        Op::Forward => json!({ "url": s.forward().await? }),
        Op::Reload => json!({ "url": s.reload().await? }),
        Op::CurrentUrl => json!({ "url": s.current_url().await? }),
        Op::Tabs => json!({ "tabs": s.tabs().await? }),
        Op::NewTab { url } => json!({ "id": s.new_tab(&url).await? }),
        Op::SelectTab { id } => json!({ "ok": s.select_tab(&id).await? }),
        Op::CloseTab { id } => json!({ "ok": s.close_tab(&id).await? }),
        Op::Snapshot => {
            serde_json::to_value(s.snapshot().await?).map_err(|e| bad(e.to_string()))?
        }
        Op::Read => json!({ "text": s.read().await? }),
        Op::Click { r#ref } => json!({ "ok": s.click(&r#ref).await? }),
        Op::Fill { r#ref, text } => json!({ "ok": s.fill(&r#ref, &text).await? }),
        Op::FillSecret {
            r#ref,
            secret_ref,
            expect_origin,
        } => {
            let resolver = app.resolver.as_ref().ok_or_else(|| {
                ApiError::from(Error::Unsupported("no --secret-command configured".into()))
            })?;
            s.fill_secret(&r#ref, &secret_ref, resolver, expect_origin.as_deref())
                .await?;
            json!({ "ok": true })
        }
        Op::Focus { r#ref } => json!({ "ok": s.focus(&r#ref).await? }),
        Op::Press { key, hold_ms } => {
            match hold_ms.filter(|ms| *ms > 0) {
                Some(ms) => {
                    s.press_and_hold(&key, std::time::Duration::from_millis(ms))
                        .await?
                }
                None => s.press(&key).await?,
            }
            json!({ "ok": true })
        }
        Op::KeyDown { key } => {
            s.key_down(&key).await?;
            json!({ "held": s.held_keys().await? })
        }
        Op::KeyUp { key } => {
            s.key_up(&key).await?;
            json!({ "held": s.held_keys().await? })
        }
        Op::ReleaseKeys => json!({ "ok": s.release_keys().await? }),
        Op::Eval { expression } => json!({ "value": s.eval(&expression).await? }),
        Op::WebmcpList => json!(s.webmcp_list().await?),
        Op::WebmcpCall {
            tool_ref,
            arguments,
            confirmed,
            timeout_ms,
        } => {
            json!(
                s.webmcp_call(&tool_ref, arguments, confirmed, timeout_ms)
                    .await?
            )
        }
        Op::Html { r#ref } => json!({ "html": s.html(r#ref.as_deref()).await? }),
        Op::Cookies { urls } => json!({ "cookies": s.cookies(&urls).await? }),
        Op::SetCookies { cookies } => json!({ "ok": s.set_cookies(&cookies).await? }),
        Op::DeleteCookies { name, url } => {
            json!({ "ok": s.delete_cookies(&name, url.as_deref()).await? })
        }
        Op::ClearCookies => json!({ "ok": s.clear_cookies().await? }),
        Op::PageScreenshot { full_page } => {
            let shot = s.page_screenshot(full_page).await?;
            json!({ "width": shot.width, "height": shot.height, "truncated": shot.truncated, "png_base64": png(shot.png) })
        }
        Op::Screenshot => {
            let f = s.screenshot().await?;
            json!({ "width": f.width, "height": f.height, "png_base64": png(f.to_png()?) })
        }
        Op::ClickAt {
            x,
            y,
            button,
            modifiers,
        } => {
            let button = button.unwrap_or(Button::Left);
            let mut steps = Vec::new();
            if let Some(m) = &modifiers {
                steps.push(Input::KeyDown(m.clone()));
            }
            steps.extend([
                Input::Move { x, y },
                Input::Button {
                    button,
                    pressed: true,
                },
                Input::Button {
                    button,
                    pressed: false,
                },
            ]);
            if let Some(m) = modifiers {
                steps.push(Input::KeyUp(m));
            }
            json!({ "ok": s.input(steps).await? })
        }
        Op::DesktopWindows => json!({ "windows": s.desktop_windows().await? }),
        Op::DesktopSnapshot {
            max_depth,
            max_nodes,
            pid,
            root,
            offset,
        } => {
            let defaults = TreeOptions::default();
            let options = TreeOptions {
                max_depth: max_depth.unwrap_or(defaults.max_depth),
                max_nodes: max_nodes.unwrap_or(defaults.max_nodes),
                pid,
            };
            serde_json::to_value(
                s.desktop_snapshot_with(options, root.as_deref(), offset.unwrap_or(0))
                    .await?,
            )
            .map_err(|e| bad(e.to_string()))?
        }
        Op::DesktopCapabilities => {
            serde_json::to_value(s.desktop_capabilities()?).map_err(|e| bad(e.to_string()))?
        }
        Op::DesktopFocus { target } => {
            serde_json::to_value(s.desktop_focus(&target).await?).map_err(|e| bad(e.to_string()))?
        }
        Op::DesktopSetValue { r#ref, value } => {
            s.desktop_set_value(&r#ref, value).await?;
            json!({ "ok": true })
        }
        Op::DesktopClickRef {
            r#ref,
            button,
            modifiers,
        } => {
            s.desktop_click_ref(&r#ref, button.unwrap_or(Button::Left), modifiers.as_deref())
                .await?;
            json!({ "ok": true })
        }
        Op::DesktopDoubleClickRef { r#ref } => {
            s.desktop_double_click_ref(&r#ref).await?;
            json!({ "ok": true })
        }
        Op::Key { chord } => json!({ "ok": s.key(&chord).await? }),
        Op::HoldKey { chord, ms } => {
            json!({ "ok": s.hold_key(&chord, std::time::Duration::from_millis(ms)).await? })
        }
        Op::Type { text } => json!({ "ok": s.type_text(&text).await? }),
        Op::Scroll { x, y, dx, dy } => json!({ "ok": s.scroll(x, y, dx, dy).await? }),
        Op::PointerPosition => {
            let p = s.pointer_position()?;
            json!({ "x": p.map(|p| p.0), "y": p.map(|p| p.1) })
        }
        Op::ClipboardGet => json!({ "text": s.clipboard_text().await? }),
        Op::ClipboardSet { text } => json!({ "ok": s.set_clipboard_text(&text).await? }),
        Op::HandBack => json!({ "hand_back": s.take_hand_back()?.map(|h| format!("{h:?}")) }),
        Op::Release => json!({ "released": s.release() }),
    };
    Ok(Json(v))
}

fn random_id() -> Result<String, ApiError> {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b)
        .map_err(|e| ApiError::from(Error::Io(std::io::Error::other(e.to_string()))))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

// ---- compact CLI client -----------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedSession {
    endpoint: String,
    session_id: String,
}

struct DaemonClient {
    base_url: url::Url,
    normalized_endpoint: String,
    host_header: String,
    connect_addr: String,
    token: String,
    principal: String,
    session_override: Option<String>,
    session_file: PathBuf,
}

impl DaemonClient {
    fn from_args(args: &ClientArgs) -> Result<Self, String> {
        let base_url = url::Url::parse(&args.endpoint)
            .map_err(|e| format!("invalid --endpoint {:?}: {e}", args.endpoint))?;
        if base_url.scheme() != "http" {
            return Err(format!(
                "unsupported endpoint scheme {:?}: daemon client expects http://",
                base_url.scheme()
            ));
        }
        let host = base_url
            .host_str()
            .ok_or_else(|| format!("--endpoint {:?} has no host", args.endpoint))?;
        let port = base_url.port_or_known_default().unwrap_or(80);
        let connect_addr = format!("{host}:{port}");
        let host_header = match base_url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_owned(),
        };
        let normalized_endpoint = base_url.as_str().trim_end_matches('/').to_owned();
        let token = args
            .token
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "set AGENT_WORKSTATION_TOKEN or pass --token to authenticate with the daemon"
                    .to_owned()
            })?;
        let session_file = match &args.session_file {
            Some(p) => p.clone(),
            None => default_session_file()?,
        };
        Ok(Self {
            base_url,
            normalized_endpoint,
            host_header,
            connect_addr,
            token,
            principal: args.principal.clone(),
            session_override: args.session.clone(),
            session_file,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        let base_path = self.base_url.path().trim_end_matches('/');
        let full_path = format!("{base_path}{path}");
        let stream = TcpStream::connect(&self.connect_addr)
            .await
            .map_err(|e| format!("cannot connect to daemon at {}: {e}", self.connect_addr))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| format!("HTTP handshake with {} failed: {e}", self.connect_addr))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let payload = match body {
            Some(v) => serde_json::to_vec(v).map_err(|e| format!("encode request JSON: {e}"))?,
            None => Vec::new(),
        };
        let mut req_builder = hyper::Request::builder()
            .method(method)
            .uri(&full_path)
            .header("host", &self.host_header)
            .header("authorization", format!("Bearer {}", self.token))
            .header("connection", "close");
        if body.is_some() {
            req_builder = req_builder.header("content-type", "application/json");
        }
        let req = req_builder
            .body(Full::new(HyperBytes::from(payload)))
            .map_err(|e| format!("build HTTP request: {e}"))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("request {method} {full_path} failed: {e}"))?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("read response body: {e}"))?
            .to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok((status, value))
    }

    async fn checked(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let (status, value) = self.request(method, path, body).await?;
        if status.is_success() {
            return Ok(value);
        }
        Err(format_daemon_error(status, &value))
    }

    async fn call_op(&self, session_id: &str, mut op: Value) -> Result<Value, String> {
        let obj = op
            .as_object_mut()
            .ok_or_else(|| "internal error: op payload must be a JSON object".to_owned())?;
        obj.insert("principal".into(), Value::String(self.principal.clone()));
        self.checked("POST", &format!("/sessions/{session_id}/call"), Some(&op))
            .await
    }

    fn read_saved_session(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.session_file).ok()?;
        let saved: SavedSession = serde_json::from_str(&text).ok()?;
        (saved.endpoint == self.normalized_endpoint && !saved.session_id.is_empty())
            .then_some(saved.session_id)
    }

    fn write_saved_session(&self, session_id: &str) -> Result<(), String> {
        if let Some(parent) = self.session_file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create session directory {}: {e}", parent.display()))?;
        }
        let saved = SavedSession {
            endpoint: self.normalized_endpoint.clone(),
            session_id: session_id.to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
        let tmp = self.session_file.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.session_file)
            .map_err(|e| format!("save session file {}: {e}", self.session_file.display()))
    }

    fn clear_saved_session_if(&self, session_id: &str) {
        if self.read_saved_session().as_deref() == Some(session_id) {
            let _ = std::fs::remove_file(&self.session_file);
        }
    }

    async fn list_session_ids(&self) -> Result<Vec<String>, String> {
        let v = self.checked("GET", "/sessions", None).await?;
        let ids = v["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
            .collect();
        Ok(ids)
    }

    async fn resolve_session(&self) -> Result<String, String> {
        if let Some(id) = &self.session_override {
            return Ok(id.clone());
        }
        if let Some(saved) = self.read_saved_session() {
            return Ok(saved);
        }
        let ids = self.list_session_ids().await?;
        match ids.as_slice() {
            [] => Err(
                "no active session; run 'agent-workstation open <url>' or pass --session <id>"
                    .to_owned(),
            ),
            [only] => Ok(only.clone()),
            many => Err(format!(
                "multiple active sessions ({}); pass --session <id>",
                many.join(", ")
            )),
        }
    }
}

fn format_daemon_error(status: StatusCode, value: &Value) -> String {
    if let Some(obj) = value.as_object() {
        if obj.get("code").and_then(Value::as_str) == Some("confirmation_required") {
            return value.to_string();
        }
        let code = obj.get("code").and_then(Value::as_str);
        let msg = obj.get("message").and_then(Value::as_str);
        match (code, msg) {
            (Some(c), Some(m)) => return format!("{c}: {m} (HTTP {})", status.as_u16()),
            (Some(c), None) => return format!("{c} (HTTP {})", status.as_u16()),
            _ => {}
        }
    }
    if let Some(s) = value.as_str() {
        return format!("HTTP {}: {s}", status.as_u16());
    }
    format!("HTTP {}: {value}", status.as_u16())
}

fn default_session_file() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir)
            .join("agent-workstation")
            .join("session.json"));
    }
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir)
            .join("agent-workstation")
            .join("session.json"));
    }
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(home)
            .join(".cache")
            .join("agent-workstation")
            .join("session.json"));
    }
    Err(
        "cannot determine default session file; set XDG_RUNTIME_DIR, HOME, or --session-file"
            .into(),
    )
}

fn default_profile_dir(session_file: &Path, profile: &str) -> PathBuf {
    session_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("profiles")
        .join(profile)
}

fn write_png_output(path: Option<&Path>, resp: &Value, as_json: bool) -> Result<(), String> {
    let Some(path) = path else {
        println!(
            "{}",
            serde_json::to_string_pretty(resp).map_err(|e| e.to_string())?
        );
        return Ok(());
    };
    let encoded = resp["png_base64"]
        .as_str()
        .ok_or_else(|| "daemon screenshot response missing png_base64".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("decode screenshot PNG: {e}"))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create screenshot directory {}: {e}", parent.display()))?;
    }
    std::fs::write(path, &bytes)
        .map_err(|e| format!("write screenshot {}: {e}", path.display()))?;
    let width = resp["width"].as_u64().unwrap_or(0);
    let height = resp["height"].as_u64().unwrap_or(0);
    let truncated = resp["truncated"].as_bool();
    if as_json {
        let mut meta = json!({
            "path": path.display().to_string(),
            "width": width,
            "height": height,
            "bytes": bytes.len(),
        });
        if let Some(t) = truncated {
            meta["truncated"] = Value::Bool(t);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "saved {} ({width}x{height}, {} bytes)",
            path.display(),
            bytes.len()
        );
    }
    Ok(())
}

fn print_takeover_status(status: &Value, as_json: bool) -> Result<(), String> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(status).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let holder = status["holder"].as_str().unwrap_or("unknown");
    println!("holder={holder}");
    if let Some(url) = status["takeover_url"].as_str().filter(|u| !u.is_empty()) {
        println!("takeover_url={url}");
    }
    if let Some(running) = status["browser_running"].as_bool() {
        println!("browser_running={running}");
    }
    if let Some(remaining) = status["idle_remaining_ms"].as_u64() {
        println!("idle_remaining_ms={remaining}");
    }
    Ok(())
}

async fn run_client(args: ClientArgs, cmd: Command) -> Result<(), String> {
    let client = DaemonClient::from_args(&args)?;
    let as_json = args.json;
    match cmd {
        Command::Open {
            url,
            profile,
            profile_dir,
            proxy,
            new,
        } => {
            if new && client.session_override.is_some() {
                return Err("cannot combine --new with --session".into());
            }
            let (session_id, created, takeover_url) = if new {
                let dir = profile_dir
                    .unwrap_or_else(|| default_profile_dir(&client.session_file, &profile));
                let body = json!({
                    "profile": profile,
                    "profile_dir": dir,
                    "proxy": proxy,
                });
                let resp = client.checked("POST", "/sessions", Some(&body)).await?;
                let id = resp["id"]
                    .as_str()
                    .ok_or_else(|| "daemon create session response missing id".to_owned())?
                    .to_owned();
                let takeover = resp["takeover_url"].as_str().map(str::to_owned);
                client.write_saved_session(&id)?;
                (id, true, takeover)
            } else if let Some(explicit) = &client.session_override {
                (explicit.clone(), false, None)
            } else {
                let active = client.list_session_ids().await?;
                let reusable = client
                    .read_saved_session()
                    .filter(|id| active.iter().any(|a| a == id))
                    .or_else(|| (active.len() == 1).then(|| active[0].clone()));
                if let Some(id) = reusable {
                    client.write_saved_session(&id)?;
                    (id, false, None)
                } else {
                    let dir = profile_dir
                        .unwrap_or_else(|| default_profile_dir(&client.session_file, &profile));
                    let body = json!({
                        "profile": profile,
                        "profile_dir": dir,
                        "proxy": proxy,
                    });
                    let resp = client.checked("POST", "/sessions", Some(&body)).await?;
                    let id = resp["id"]
                        .as_str()
                        .ok_or_else(|| "daemon create session response missing id".to_owned())?
                        .to_owned();
                    let takeover = resp["takeover_url"].as_str().map(str::to_owned);
                    client.write_saved_session(&id)?;
                    (id, true, takeover)
                }
            };
            let nav = client
                .call_op(&session_id, json!({ "op": "navigate", "url": url }))
                .await?;
            let final_url = nav["url"].as_str().unwrap_or(&url);
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "session": session_id,
                        "created": created,
                        "url": final_url,
                        "takeover_url": takeover_url,
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                if created {
                    println!("session={session_id}");
                    if let Some(tu) = takeover_url.filter(|u| !u.is_empty()) {
                        println!("takeover_url={tu}");
                    }
                }
                println!("{final_url}");
            }
        }
        Command::Snapshot => {
            let id = client.resolve_session().await?;
            let resp = client.call_op(&id, json!({ "op": "snapshot" })).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                );
            } else {
                let snap_id = resp["id"].as_str().unwrap_or("?");
                let url = resp["url"].as_str().unwrap_or("");
                let refs = resp["refs"].as_u64().unwrap_or(0);
                let text = resp["text"].as_str().unwrap_or("");
                println!("# snapshot {snap_id} ({url}, {refs} refs)");
                if !text.is_empty() {
                    println!("{text}");
                }
            }
        }
        Command::Click { target } => {
            let id = client.resolve_session().await?;
            let resp = client
                .call_op(&id, json!({ "op": "click", "ref": target }))
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                );
            } else {
                println!("ok");
            }
        }
        Command::Fill { target, value } => {
            let id = client.resolve_session().await?;
            let resp = client
                .call_op(&id, json!({ "op": "fill", "ref": target, "text": value }))
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                );
            } else {
                println!("ok");
            }
        }
        Command::FillSecret {
            target,
            secret_handle,
            expect_origin,
        } => {
            let id = client.resolve_session().await?;
            let resp = client
                .call_op(
                    &id,
                    json!({
                        "op": "fill_secret",
                        "ref": target,
                        "secret_ref": secret_handle,
                        "expect_origin": expect_origin,
                    }),
                )
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                );
            } else {
                println!("ok");
            }
        }
        Command::Eval { js } => {
            let id = client.resolve_session().await?;
            let resp = client
                .call_op(&id, json!({ "op": "eval", "expression": js }))
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&resp["value"]).map_err(|e| e.to_string())?
                );
            }
        }
        Command::WebmcpList => {
            let id = client.resolve_session().await?;
            let response = client.call_op(&id, json!({ "op": "webmcp_list" })).await?;
            println!("{response}");
        }
        Command::WebmcpCall {
            tool_ref,
            arguments,
            confirmed,
            timeout_ms,
        } => {
            let arguments: Value = serde_json::from_str(&arguments)
                .map_err(|_| "WebMCP arguments must be valid JSON".to_owned())?;
            let id = client.resolve_session().await?;
            let response = client
                .call_op(
                    &id,
                    json!({
                        "op": "webmcp_call", "ref": tool_ref, "arguments": arguments,
                        "confirmed": confirmed, "timeout_ms": timeout_ms,
                    }),
                )
                .await?;
            println!("{response}");
        }
        Command::Screenshot {
            path,
            full_page,
            desktop,
        } => {
            let id = client.resolve_session().await?;
            let op = if desktop {
                json!({ "op": "screenshot" })
            } else {
                json!({ "op": "page_screenshot", "full_page": full_page })
            };
            let resp = client.call_op(&id, op).await?;
            write_png_output(path.as_deref(), &resp, as_json)?;
        }
        Command::Desktop { host: true, action } => {
            run_host_desktop(action, as_json).await?;
        }
        Command::Desktop {
            host: false,
            action,
        } => {
            let id = client.resolve_session().await?;
            match action {
                DesktopCommand::Capabilities => {
                    let resp = client
                        .call_op(&id, json!({ "op": "desktop_capabilities" }))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!(
                            "platform={} backend={} tree={} actions={} keys={} pointer={} windows={}",
                            resp["platform"].as_str().unwrap_or("?"),
                            resp["backend"].as_str().unwrap_or("?"),
                            resp["accessibility_tree"].as_bool().unwrap_or(false),
                            resp["native_actions"].as_bool().unwrap_or(false),
                            resp["keyboard_injection"].as_bool().unwrap_or(false),
                            resp["pointer_injection"].as_bool().unwrap_or(false),
                            resp["window_enumeration"].as_bool().unwrap_or(false),
                        );
                    }
                }
                DesktopCommand::Snapshot {
                    max_depth,
                    max_nodes,
                    pid,
                    root,
                    offset,
                } => {
                    let resp = client
                        .call_op(&id, json!({ "op": "desktop_snapshot", "max_depth": max_depth, "max_nodes": max_nodes, "pid": pid, "root": root, "offset": offset }))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        let mode = resp["mode"].as_str().unwrap_or("unknown");
                        let snap_id = resp["snapshot_id"]
                            .as_str()
                            .or_else(|| resp["id"].as_str())
                            .unwrap_or("?");
                        let refs = resp["total_nodes"]
                            .as_u64()
                            .or_else(|| resp["refs"].as_u64())
                            .unwrap_or(0);
                        let text = resp["text"].as_str().unwrap_or("");
                        println!("# desktop snapshot {snap_id} ({refs} refs, {mode})");
                        if let Some(reason) = resp["fallback_reason"].as_str() {
                            println!("# window-only fallback: {reason}");
                            if resp["truncated"] == true {
                                println!("# truncated: use --offset to see more windows");
                            }
                        } else if resp["truncated"] == true {
                            println!(
                                "# use --root with a current ref, or --offset to page siblings"
                            );
                        }
                        if !text.is_empty() {
                            println!("{text}");
                        }
                    }
                }
                DesktopCommand::Click {
                    target,
                    y,
                    button,
                    modifiers,
                } => {
                    let parsed =
                        parse_desktop_click_target(&target, y).map_err(|e| e.to_string())?;
                    let btn: Option<Button> = button.map(Into::into);
                    let op = match parsed {
                        DesktopClickTarget::Coords { x, y } => json!({
                            "op": "click_at",
                            "x": x,
                            "y": y,
                            "button": btn,
                            "modifiers": modifiers,
                        }),
                        DesktopClickTarget::Ref(r) => json!({
                            "op": "desktop_click_ref",
                            "ref": r,
                            "button": btn,
                            "modifiers": modifiers,
                        }),
                    };
                    let resp = client.call_op(&id, op).await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::DoubleClick { target } => {
                    let resp = client
                        .call_op(
                            &id,
                            json!({ "op": "desktop_double_click_ref", "ref": target }),
                        )
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::SetValue { target, value } => {
                    let resp = client
                        .call_op(
                            &id,
                            json!({ "op": "desktop_set_value", "ref": target, "value": value }),
                        )
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::Type { text } => {
                    let resp = client
                        .call_op(&id, json!({ "op": "type", "text": text }))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::Key { chord, hold_ms } => {
                    let op = match hold_ms.filter(|ms| *ms > 0) {
                        Some(ms) => json!({ "op": "hold_key", "chord": chord, "ms": ms }),
                        None => json!({ "op": "key", "chord": chord }),
                    };
                    let resp = client.call_op(&id, op).await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::Scroll { x, y, dx, dy } => {
                    let resp = client
                        .call_op(
                            &id,
                            json!({ "op": "scroll", "x": x, "y": y, "dx": dx, "dy": dy }),
                        )
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("ok");
                    }
                }
                DesktopCommand::Windows => {
                    let resp = client
                        .call_op(&id, json!({ "op": "desktop_windows" }))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else if let Some(windows) = resp["windows"].as_array() {
                        for w in windows {
                            let wid = w["id"].as_u64().unwrap_or(0);
                            let focused = if w["focused"].as_bool().unwrap_or(false) {
                                "*"
                            } else {
                                "-"
                            };
                            let app = w["app_id"].as_str().unwrap_or("-");
                            let name = w["name"].as_str().unwrap_or("");
                            let r = &w["rect"];
                            println!(
                                "{wid}\t{focused}\t{app}\t{},{} {}x{}\t{name}",
                                r["x"].as_i64().unwrap_or(0),
                                r["y"].as_i64().unwrap_or(0),
                                r["width"].as_u64().unwrap_or(0),
                                r["height"].as_u64().unwrap_or(0),
                            );
                        }
                    }
                }
                DesktopCommand::Focus { target } => {
                    let resp = client
                        .call_op(&id, json!({ "op": "desktop_focus", "target": target }))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else if let Some(target) = resp["target"].as_str() {
                        println!("focused {target}");
                    } else {
                        let wid = resp["window"]["id"].as_u64().unwrap_or(0);
                        let name = resp["window"]["name"].as_str().unwrap_or("");
                        println!("focused {wid} {name:?}");
                    }
                }
                DesktopCommand::Screenshot { path } => {
                    let resp = client.call_op(&id, json!({ "op": "screenshot" })).await?;
                    write_png_output(path.as_deref(), &resp, as_json)?;
                }
            }
        }
        Command::Takeover { action } => {
            let id = client.resolve_session().await?;
            match action.unwrap_or(TakeoverCommand::Status) {
                TakeoverCommand::Status => {
                    let resp = client
                        .checked("GET", &format!("/sessions/{id}/takeover"), None)
                        .await?;
                    print_takeover_status(&resp, as_json)?;
                }
                TakeoverCommand::Start { label } => {
                    let body = json!({ "principal": label });
                    let resp = client
                        .checked(
                            "POST",
                            &format!("/sessions/{id}/takeover/start"),
                            Some(&body),
                        )
                        .await?;
                    print_takeover_status(&resp, as_json)?;
                }
                TakeoverCommand::End { note } => {
                    let body = json!({ "note": note });
                    let resp = client
                        .checked("POST", &format!("/sessions/{id}/takeover/end"), Some(&body))
                        .await?;
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
                        );
                    } else {
                        let released = resp["released"].as_bool().unwrap_or(false);
                        let holder = resp["status"]["holder"].as_str().unwrap_or("unknown");
                        println!("released={released} holder={holder}");
                    }
                }
            }
        }
        Command::Close { force } => {
            let id = client.resolve_session().await?;
            let path = if force {
                format!("/sessions/{id}")
            } else {
                format!("/sessions/{id}?if_agent=true")
            };
            client.checked("DELETE", &path, None).await?;
            client.clear_saved_session_if(&id);
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "closed": true,
                        "session": id,
                        "force": force,
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                println!("closed {id}");
            }
        }
        Command::Daemon(_) | Command::Skills { .. } => unreachable!(),
    }
    Ok(())
}

async fn run_daemon(daemon_args: DaemonArgs, token: Option<String>) {
    let opts = match Options::from_daemon_args(daemon_args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("agent-workstation: {e}");
            std::process::exit(2);
        }
    };
    let Some(token) = token.filter(|t| t.len() >= 16) else {
        eprintln!("agent-workstation: set AGENT_WORKSTATION_TOKEN (16+ characters)");
        std::process::exit(2);
    };
    let listen = opts.listen;
    let app = Arc::new(App {
        token,
        resolver: opts.secret_command.clone().map(CommandResolver),
        opts,
        registry: Mutex::new(Registry::default()),
        changed: Notify::new(),
    });
    let router = Router::new()
        .route("/healthz", get(health))
        .route(
            "/sessions",
            get(sessions).merge(post(create).route_layer(middleware::from_fn_with_state(
                (Arc::clone(&app), false),
                supervise,
            ))),
        )
        .route("/quiesce", post(quiesce_all))
        .route("/resume", post(resume_all))
        .route("/sessions/{id}/quiesce", post(quiesce_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/takeover", get(takeover_status))
        .route(
            "/sessions/{id}/takeover/start",
            post(takeover_start).route_layer(middleware::from_fn_with_state(
                (Arc::clone(&app), true),
                supervise,
            )),
        )
        .route(
            "/sessions/{id}/takeover/end",
            post(takeover_end).route_layer(middleware::from_fn_with_state(
                (Arc::clone(&app), true),
                supervise,
            )),
        )
        .route(
            "/sessions/{id}",
            delete(remove).route_layer(middleware::from_fn_with_state(
                (Arc::clone(&app), true),
                supervise,
            )),
        )
        .route(
            "/sessions/{id}/call",
            post(call).route_layer(middleware::from_fn_with_state(
                (Arc::clone(&app), true),
                supervise,
            )),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            authenticate,
        ))
        .with_state(app);
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-workstation: cannot listen on {listen}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "agent-workstation listening on {}",
        listener.local_addr().expect("listener address")
    );
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("agent-workstation: {e}");
        std::process::exit(1);
    }
}

async fn run_host_desktop(action: DesktopCommand, as_json: bool) -> Result<(), String> {
    let adapter = native_adapter();
    match action {
        DesktopCommand::Capabilities => {
            let caps = adapter.capabilities();
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&caps).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "platform={} backend={} tree={} actions={} keys={} pointer={} windows={}",
                    caps.platform,
                    caps.backend,
                    caps.accessibility_tree,
                    caps.native_actions,
                    caps.keyboard_injection,
                    caps.pointer_injection,
                    caps.window_enumeration,
                );
            }
        }
        DesktopCommand::Snapshot {
            max_depth,
            max_nodes,
            pid,
            root,
            offset,
        } => {
            if root.is_some() || offset.is_some() {
                return Err("--root and --offset are for daemon session snapshots".into());
            }
            let defaults = TreeOptions::default();
            let opts = TreeOptions {
                max_depth: max_depth.unwrap_or(defaults.max_depth),
                max_nodes: max_nodes.unwrap_or(defaults.max_nodes),
                pid,
            };
            let tree = adapter
                .snapshot(opts)
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "# host desktop snapshot {} ({}, {} nodes{})",
                    tree.snapshot_id,
                    tree.platform,
                    tree.total_nodes,
                    if tree.truncated { ", truncated" } else { "" }
                );
                if !tree.text.is_empty() {
                    println!("{}", tree.text);
                }
            }
        }
        DesktopCommand::Windows => {
            let windows = adapter
                .list_windows()
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "windows": windows }))
                        .map_err(|e| e.to_string())?
                );
            } else {
                for w in windows {
                    let focused = if w.focused { "*" } else { "-" };
                    let app = w.app_id.as_deref().unwrap_or("-");
                    println!(
                        "{}\t{focused}\t{app}\t{},{} {}x{}\t{}",
                        w.id, w.rect.x, w.rect.y, w.rect.width, w.rect.height, w.name
                    );
                }
            }
        }
        DesktopCommand::Type { text } => {
            adapter
                .act(DesktopAction::TypeText { text })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
            if as_json {
                println!("{}", json!({ "ok": true }));
            } else {
                println!("ok");
            }
        }
        DesktopCommand::Key { chord, .. } => {
            adapter
                .act(DesktopAction::PressKey { chord })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
            if as_json {
                println!("{}", json!({ "ok": true }));
            } else {
                println!("ok");
            }
        }
        DesktopCommand::Scroll { dx, dy, .. } => {
            adapter
                .act(DesktopAction::Scroll {
                    target: None,
                    dx,
                    dy,
                })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
            if as_json {
                println!("{}", json!({ "ok": true }));
            } else {
                println!("ok");
            }
        }
        DesktopCommand::Click { target, button, .. } => {
            let node_ref = NodeRef::parse(&target).map_err(|e| format!("{}: {e}", e.code()))?;
            adapter
                .act(DesktopAction::Click {
                    target: node_ref,
                    button: button.map(Into::into),
                })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
        }
        DesktopCommand::DoubleClick { target } => {
            let node_ref = NodeRef::parse(&target).map_err(|e| format!("{}: {e}", e.code()))?;
            adapter
                .act(DesktopAction::DoubleClick { target: node_ref })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
        }
        DesktopCommand::Focus { target } => {
            let node_ref = NodeRef::parse(&target).map_err(|e| format!("{}: {e}", e.code()))?;
            adapter
                .act(DesktopAction::Focus { target: node_ref })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
        }
        DesktopCommand::SetValue { target, value } => {
            let node_ref = NodeRef::parse(&target).map_err(|e| format!("{}: {e}", e.code()))?;
            adapter
                .act(DesktopAction::SetValue {
                    target: node_ref,
                    value,
                })
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
        }
        DesktopCommand::Screenshot { .. } => {
            return Err(
                "host PlatformAdapter uses structured accessibility trees rather than screenshots; omit --host to capture the Wayland daemon display"
                    .into(),
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        None => run_daemon(cli.daemon, cli.client.token).await,
        Some(Command::Daemon(args)) => run_daemon(args, cli.client.token).await,
        Some(Command::Skills { topic, list }) => {
            print!("{}", agent_workstation::skills::render(topic, list));
        }
        Some(Command::Desktop { host: true, action }) => {
            if let Err(e) = run_host_desktop(action, cli.client.json).await {
                eprintln!("agent-workstation: {e}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            if let Err(e) = run_client(cli.client, other).await {
                eprintln!("agent-workstation: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn panic_unwind_releases_admission_and_allows_immediate_drain() {
        let app = Arc::new(App {
            token: "daemon-test-token-0123456789".into(),
            opts: Options {
                listen: "127.0.0.1:0".parse().unwrap(),
                state_dir: PathBuf::new(),
                browser: BrowserConfig::new("/unused/browser"),
                secret_command: None,
            },
            resolver: None,
            registry: Mutex::new(Registry::default()),
            changed: Notify::new(),
        });
        let (guard, _) = app
            .admit(None)
            .unwrap_or_else(|_| panic!("admission failed"));
        assert_eq!(app.registry.lock().unwrap().daemon.in_flight, 1);
        let task = tokio::spawn(async move {
            let _guard = guard;
            panic!("exercise admission unwind");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(app.registry.lock().unwrap().daemon.in_flight, 0);
        let started = Instant::now();
        let result = drain(&app, None, Body::empty(), started)
            .await
            .unwrap_or_else(|_| panic!("drain failed after unwind"));
        assert_eq!(result.0["quiesced"], true);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
