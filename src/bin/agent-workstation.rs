//! Standalone daemon: desktop sessions over a small JSON HTTP API.
//!
//! ```text
//! agent-workstation --listen 127.0.0.1:7600 --state-dir /var/lib/agent-workstation \
//!     --browser /usr/bin/chromium [--seed-flag --fingerprint] [--browser-arg ARG]... \
//!     [--secret-command CMD [ARG...] --]
//! ```
//!
//! The bearer token comes from `AGENT_WORKSTATION_TOKEN` (required). With
//! `--secret-command`, `fill_secret` runs `CMD ARG... <secret_ref>` and uses its
//! standard output (one trailing newline trimmed) as the value.
//!
//! - `POST /sessions` `{"profile": "name", "profile_dir": "/path", "proxy": "http://…"?}` → `{"id", "takeover_url"}`
//! - `POST /sessions/{id}/call` `{"principal": "agent-a", "op": "navigate", "url": "https://example.com"}`
//!   → result JSON. The first principal to call holds the session until it
//!   calls `{"op": "release"}`; another principal gets `409` with code
//!   `leased`.
//! - `DELETE /sessions/{id}`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_workstation::{
    BrowserConfig, Button, Cookie, DesktopSession, Error, Input, Profile, Proxy, Secret,
    SecretResolver, SessionConfig,
};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

#[derive(Debug)]
struct Options {
    listen: std::net::SocketAddr,
    state_dir: PathBuf,
    browser: BrowserConfig,
    secret_command: Option<Vec<String>>,
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let (mut listen, mut state, mut browser, mut seed, mut secret) = (None, None, None, None, None);
    let mut browser_args = Vec::new();
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--listen" => listen = Some(val()?.parse().map_err(|e| format!("--listen: {e}"))?),
            "--state-dir" => state = Some(PathBuf::from(val()?)),
            "--browser" => browser = Some(PathBuf::from(val()?)),
            "--seed-flag" => seed = Some(val()?),
            "--browser-arg" => browser_args.push(val()?),
            "--secret-command" => {
                let cmd: Vec<String> = args.by_ref().take_while(|x| x != "--").collect();
                if cmd.is_empty() {
                    return Err("--secret-command needs a command".into());
                }
                secret = Some(cmd);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mut cfg = BrowserConfig::new(browser.ok_or("--browser is required")?);
    cfg.seed_flag = seed;
    cfg.extra_args = browser_args;
    Ok(Options {
        listen: listen.unwrap_or_else(|| ([127, 0, 0, 1], 7600).into()),
        state_dir: state.ok_or("--state-dir is required")?,
        browser: cfg,
        secret_command: secret,
    })
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
    sessions: RwLock<HashMap<String, Arc<DesktopSession>>>,
}

type Shared = Arc<App>;

struct ApiError(StatusCode, Value);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        let status = match &e {
            Error::HumanInControl { .. } | Error::Leased { .. } => StatusCode::CONFLICT,
            Error::StaleRef(_) => StatusCode::CONFLICT,
            Error::Closed => StatusCode::GONE,
            Error::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Refused(_) => StatusCode::FORBIDDEN,
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
    headers: HeaderMap,
    Json(req): Json<CreateSession>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
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
    app.sessions
        .write()
        .await
        .insert(id.clone(), Arc::new(session));
    Ok(Json(json!({ "id": id, "takeover_url": url })))
}

async fn remove(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    let s = app
        .sessions
        .write()
        .await
        .remove(&id)
        .ok_or_else(|| ApiError::from(Error::NotFound(id.clone())))?;
    s.close().await;
    Ok(StatusCode::NO_CONTENT)
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

async fn call(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(Call { principal, op }): Json<Call>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let session = app
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::from(Error::NotFound(id.clone())))?;
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
    use std::io::Read;
    let mut b = [0u8; 12];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| ApiError::from(Error::Io(e)))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[tokio::main]
async fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("agent-workstation: {e}");
            std::process::exit(2);
        }
    };
    let Some(token) = std::env::var("AGENT_WORKSTATION_TOKEN")
        .ok()
        .filter(|t| t.len() >= 16)
    else {
        eprintln!("agent-workstation: set AGENT_WORKSTATION_TOKEN (16+ characters)");
        std::process::exit(2);
    };
    let listen = opts.listen;
    let app = Arc::new(App {
        token,
        resolver: opts.secret_command.clone().map(CommandResolver),
        opts,
        sessions: RwLock::new(HashMap::new()),
    });
    let router = Router::new()
        .route("/sessions", post(create))
        .route("/sessions/{id}", delete(remove))
        .route("/sessions/{id}/call", post(call))
        .with_state(app);
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-workstation: cannot listen on {listen}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("agent-workstation listening on {listen}");
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("agent-workstation: {e}");
        std::process::exit(1);
    }
}
