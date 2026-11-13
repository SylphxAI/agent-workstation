//! Full-desktop takeover: noVNC page and WebSocket bridge to the VNC server.
//!
//! [`Takeover`] serves an embedded noVNC page and bridges its WebSocket to a
//! VNC server listening on a unix socket (`wayvnc --unix-socket`). Every
//! endpoint requires the session token (query `token`, `Authorization:
//! Bearer`, or the HttpOnly cookie the page sets). The bridge parses the RFB
//! client stream and forwards keyboard, pointer and clipboard messages only
//! while [`ControlGate::human_may_input`] holds; anything it cannot parse
//! closes the connection. The VNC server must offer security type None: the
//! token is the authentication.
//!
//! Endpoints (relative to the page URL): `ws` (binary RFB), `state` (JSON),
//! `events` (server-sent `state` events), `take` and `release` (POST,
//! `{"note": "..."}` of at most [`MAX_NOTE_CHARS`] characters).
//!
//! The page can be served two ways, together or alone: on its own listener
//! ([`TakeoverConfig::bind`]), and mounted in the caller's HTTP server under
//! any path prefix ([`Takeover::router`]). Where people reach it is the
//! public URL, which may change while the session runs
//! ([`Takeover::set_public_url`]); the page accepts requests from that
//! origin and scopes its cookie to that path.

use std::convert::Infallible;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::Utc;
use futures::{SinkExt, Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use url::Url;

use crate::error::{Error, Result};
use crate::gate::{ControlGate, Holder, IDLE_TIMEOUT};

/// Longest hand-back note accepted, in characters.
pub const MAX_NOTE_CHARS: usize = 2000;
/// Largest ClientCutText payload the bridge buffers.
const MAX_CUT_TEXT: usize = 16 << 20;

/// Where and how the takeover server listens.
#[derive(Clone)]
pub struct TakeoverConfig {
    /// Own listener; defaults to `127.0.0.1:0`. `None` serves the page only
    /// through [`Takeover::router`], which then needs a `public_url`.
    pub bind: Option<SocketAddr>,
    /// URL people reach the page at (a reverse proxy, a tunnel, or the
    /// caller's own server where it mounts [`Takeover::router`]); defaults
    /// to `http://<local_addr>/`. Settable later with
    /// [`Takeover::set_public_url`].
    pub public_url: Option<Url>,
    /// Session token; `None` draws 32 random bytes (URL-safe base64).
    pub token: Option<String>,
}

impl Default for TakeoverConfig {
    fn default() -> Self {
        Self {
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            public_url: None,
            token: None,
        }
    }
}

impl std::fmt::Debug for TakeoverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TakeoverConfig")
            .field("bind", &self.bind)
            .field("public_url", &self.public_url.as_ref().map(Url::as_str))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A running takeover server. Dropping it stops the server too.
pub struct Takeover {
    addr: Option<SocketAddr>,
    shared: Arc<Shared>,
    app: Router,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Takeover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Takeover")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// Where people reach the page.
struct Public {
    base: Url,
    origin: Option<String>,
}

impl Public {
    fn new(mut base: Url) -> Result<Self> {
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(Error::InvalidInput(format!(
                "takeover public URL {base} must be http(s) with a host"
            )));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        base.set_query(None);
        base.set_fragment(None);
        let origin = Some(base.origin().ascii_serialization()).filter(|o| o != "null");
        Ok(Self { base, origin })
    }
}

struct Shared {
    token: String,
    cookie: String,
    public: std::sync::RwLock<Public>,
    vnc: PathBuf,
    gate: Arc<ControlGate>,
    stop: watch::Receiver<bool>,
}

impl Shared {
    fn public(&self) -> std::sync::RwLockReadGuard<'_, Public> {
        self.public.read().unwrap_or_else(|p| p.into_inner())
    }
}

impl Takeover {
    /// Bind and serve the page and bridge for the VNC server at `vnc_socket`.
    pub async fn start(
        config: TakeoverConfig,
        vnc_socket: PathBuf,
        gate: Arc<ControlGate>,
    ) -> Result<Takeover> {
        let token = match config.token {
            Some(t) if t.is_empty() => {
                return Err(Error::InvalidInput("empty takeover token".into()));
            }
            Some(t) => t,
            None => random_token()?,
        };
        let listener = match config.bind {
            Some(bind) => Some(TcpListener::bind(bind).await?),
            None => None,
        };
        let addr = listener.as_ref().map(TcpListener::local_addr).transpose()?;
        let base = match (config.public_url, addr) {
            (Some(u), _) => u,
            (None, Some(addr)) => Url::parse(&format!("http://{addr}/"))
                .map_err(|e| Error::InvalidInput(e.to_string()))?,
            (None, None) => {
                return Err(Error::InvalidInput(
                    "a takeover without its own listener needs a public URL".into(),
                ));
            }
        };
        let (stop, mut stop_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            token,
            cookie: format!("awt{}", &random_token()?[..8]),
            public: std::sync::RwLock::new(Public::new(base)?),
            vnc: vnc_socket,
            gate,
            stop: stop_rx.clone(),
        });
        let app = Router::new()
            .route("/", get(page))
            .route("/ws", get(ws))
            .route("/state", get(state))
            .route("/events", get(events))
            .route("/take", post(take))
            .route("/release", post(release))
            .route("/{*path}", get(asset))
            .layer(middleware::from_fn_with_state(shared.clone(), guard))
            .with_state(shared.clone());
        let task = listener.map(|listener| {
            let app = app.clone();
            tokio::spawn(async move {
                let shutdown = async move { stopped(&mut stop_rx).await };
                if let Err(e) = axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown)
                    .await
                {
                    tracing::warn!(error = %e, "takeover server stopped");
                }
            })
        });
        Ok(Takeover {
            addr,
            shared,
            app,
            stop,
            task,
        })
    }

    /// Page URL as people reach it, including the token.
    pub fn url(&self) -> Url {
        let mut url = self.shared.public().base.clone();
        url.query_pairs_mut()
            .clear()
            .append_pair("token", &self.shared.token);
        url
    }

    /// Change where people reach the page (a tunnel that started after the
    /// session, or the caller's mount point). Requests from the new origin
    /// are accepted from now on; the old origin's are refused unless it is
    /// the same host the request names.
    pub fn set_public_url(&self, url: Url) -> Result<()> {
        let public = Public::new(url)?;
        *self
            .shared
            .public
            .write()
            .unwrap_or_else(|p| p.into_inner()) = public;
        Ok(())
    }

    /// The page and its endpoints as a router, for mounting in the caller's
    /// own HTTP server under the public URL's path (for example with
    /// `Router::nest_service`). Every request still needs the token.
    pub fn router(&self) -> Router {
        self.app.clone()
    }

    /// Own listener's address, when it has one.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// Stop serving and close every bridge and event stream; a mounted
    /// router answers `410 Gone` from then on. The own listener's task ends
    /// on its own.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// [`Takeover::stop`], then wait for the own listener's task to end.
    pub async fn shutdown(self) {
        self.stop();
        if let Some(task) = self.task {
            let _ = task.await;
        }
    }
}

/// Resolves once [`Takeover::shutdown`] is called or the [`Takeover`] is dropped.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Every token the request presents, from any accepted place.
fn presented(headers: &HeaderMap, uri: &Uri, cookie: &str) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(q) = uri.query() {
        found.extend(
            url::form_urlencoded::parse(q.as_bytes())
                .filter(|(k, _)| k == "token")
                .map(|(_, v)| v.into_owned()),
        );
    }
    let header = |name| {
        headers
            .get_all(name)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
    };
    found.extend(
        header(header::AUTHORIZATION)
            .filter_map(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned),
    );
    for pair in header(header::COOKIE).flat_map(|v| v.split(';')) {
        if let Some((_, v)) = pair.trim().split_once('=').filter(|(k, _)| *k == cookie) {
            found.push(v.to_owned());
        }
    }
    found
}

fn reject(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        msg.to_owned(),
    )
        .into_response()
}

async fn guard(State(s): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    if *s.stop.borrow() {
        return StatusCode::GONE.into_response();
    }
    let tokens = presented(req.headers(), req.uri(), &s.cookie);
    if tokens.is_empty() {
        return reject(StatusCode::UNAUTHORIZED, "token required");
    }
    if !tokens
        .iter()
        .any(|t| ct_eq(t.as_bytes(), s.token.as_bytes()))
    {
        return reject(StatusCode::FORBIDDEN, "wrong token");
    }
    // A browser on another origin (another local port shares cookies) may not drive this one.
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .map(|o| o.to_str().unwrap_or("null"))
    {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok());
        let same_host = origin.split_once("://").map(|(_, rest)| rest) == host;
        if !same_host && s.public().origin.as_deref() != Some(origin) {
            return reject(StatusCode::FORBIDDEN, "cross-origin request");
        }
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (k, v) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'",
        ),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    res
}

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/takeover/", $path)) as &[u8])),*]
    };
}

static ASSETS: &[(&str, &[u8])] = assets! {
    "index.html", "app.js", "app.css", "novnc/LICENSE.txt", "novnc/docs/LICENSE.MPL-2.0",
    "novnc/core/base64.js", "novnc/core/crypto/aes.js", "novnc/core/crypto/bigint.js",
    "novnc/core/crypto/crypto.js", "novnc/core/crypto/des.js", "novnc/core/crypto/dh.js",
    "novnc/core/crypto/md5.js", "novnc/core/crypto/rsa.js", "novnc/core/decoders/copyrect.js",
    "novnc/core/decoders/h264.js", "novnc/core/decoders/hextile.js", "novnc/core/decoders/jpeg.js",
    "novnc/core/decoders/raw.js", "novnc/core/decoders/rre.js", "novnc/core/decoders/tight.js",
    "novnc/core/decoders/tightpng.js", "novnc/core/decoders/zlib.js", "novnc/core/decoders/zrle.js",
    "novnc/core/deflator.js", "novnc/core/display.js", "novnc/core/encodings.js",
    "novnc/core/inflator.js", "novnc/core/input/domkeytable.js", "novnc/core/input/fixedkeys.js",
    "novnc/core/input/gesturehandler.js", "novnc/core/input/keyboard.js",
    "novnc/core/input/keysym.js", "novnc/core/input/keysymdef.js", "novnc/core/input/util.js",
    "novnc/core/input/vkeys.js", "novnc/core/input/xtscancodes.js", "novnc/core/ra2.js",
    "novnc/core/rfb.js", "novnc/core/util/browser.js", "novnc/core/util/cursor.js",
    "novnc/core/util/element.js", "novnc/core/util/events.js", "novnc/core/util/eventtarget.js",
    "novnc/core/util/int.js", "novnc/core/util/logging.js", "novnc/core/util/strings.js",
    "novnc/core/websock.js", "novnc/vendor/pako/lib/utils/common.js",
    "novnc/vendor/pako/lib/zlib/adler32.js",
    "novnc/vendor/pako/lib/zlib/crc32.js", "novnc/vendor/pako/lib/zlib/deflate.js",
    "novnc/vendor/pako/lib/zlib/inffast.js",
    "novnc/vendor/pako/lib/zlib/inflate.js", "novnc/vendor/pako/lib/zlib/inftrees.js",
    "novnc/vendor/pako/lib/zlib/messages.js", "novnc/vendor/pako/lib/zlib/trees.js",
    "novnc/vendor/pako/lib/zlib/zstream.js",
};

fn serve_asset(path: &str) -> Response {
    let Some((_, body)) = ASSETS.iter().find(|(p, _)| *p == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let kind = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };
    ([(header::CONTENT_TYPE, kind)], Body::from(*body)).into_response()
}

async fn page(State(s): State<Arc<Shared>>) -> Response {
    let mut res = serve_asset("index.html");
    let cookie = {
        let public = s.public();
        let secure = if public.base.scheme() == "https" {
            "; Secure"
        } else {
            ""
        };
        format!(
            "{}={}; Path={}; HttpOnly; SameSite=Strict{secure}",
            s.cookie,
            s.token,
            public.base.path()
        )
    };
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

async fn asset(uri: Uri) -> Response {
    serve_asset(uri.path().trim_start_matches('/'))
}

/// The holder as JSON, plus a key that changes whenever a client should hear.
fn snapshot(gate: &ControlGate) -> (serde_json::Value, String) {
    match gate.holder() {
        Holder::Agent => (serde_json::json!({ "holder": "agent" }), "agent".into()),
        Holder::Human {
            since,
            principal,
            last_input,
        } => {
            let idle = (Utc::now() - last_input).to_std().unwrap_or_default();
            let remaining = IDLE_TIMEOUT.saturating_sub(idle).as_millis() as u64;
            let json = serde_json::json!({
                "holder": "human", "since": since, "principal": principal,
                "idle_timeout_ms": IDLE_TIMEOUT.as_millis() as u64, "idle_remaining_ms": remaining,
            });
            (json, format!("human {since} {last_input}"))
        }
    }
}

async fn state(State(s): State<Arc<Shared>>) -> Json<serde_json::Value> {
    Json(snapshot(&s.gate).0)
}

async fn take(State(s): State<Arc<Shared>>) -> Json<serde_json::Value> {
    s.gate.take(None);
    Json(snapshot(&s.gate).0)
}

async fn release(State(s): State<Arc<Shared>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Body {
        note: Option<String>,
    }
    let note = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Body>(&body) {
            Ok(b) => b
                .note
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty()),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("note exceeds {MAX_NOTE_CHARS} characters"),
        )
            .into_response();
    }
    s.gate.release(note);
    Json(snapshot(&s.gate).0).into_response()
}

async fn events(
    State(s): State<Arc<Shared>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let init = (s.gate.subscribe(), s.stop.clone(), None::<String>, s);
    let stream = futures::stream::unfold(init, |(mut rx, mut stop, last, s)| async move {
        loop {
            rx.borrow_and_update();
            // `holder()` also expires an idle person, so polling drives the idle hand-back.
            let (json, key) = snapshot(&s.gate);
            if last.as_ref() != Some(&key) {
                let event = Event::default().event("state").data(json.to_string());
                return Some((Ok(event), (rx, stop, Some(key), s)));
            }
            tokio::select! {
                _ = stopped(&mut stop) => return None,
                _ = rx.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn ws(State(s): State<Arc<Shared>>, up: WebSocketUpgrade) -> Response {
    up.protocols(["binary"])
        .on_upgrade(move |socket| bridge(socket, s))
}

async fn bridge(socket: WebSocket, s: Arc<Shared>) {
    let vnc = match UnixStream::connect(&s.vnc).await {
        Ok(v) => v,
        Err(e) => return tracing::warn!(error = %e, "takeover: VNC socket unreachable"),
    };
    let (mut vnc_rx, mut vnc_tx) = vnc.into_split();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut stop, mut filter, mut buf) =
        (s.stop.clone(), RfbFilter::default(), vec![0u8; 64 << 10]);
    let may_input = || s.gate.human_may_input();
    loop {
        tokio::select! {
            _ = stopped(&mut stop) => break,
            n = vnc_rx.read(&mut buf) => match n {
                Ok(n) if n > 0 => {
                    if ws_tx.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(data))) => match filter.feed(&data, &may_input) {
                    Ok((out, inputs)) => {
                        if inputs > 0 {
                            s.gate.note_input();
                        }
                        if let Some((x, y)) = filter.pointer.take() {
                            s.gate.note_pointer(f64::from(x), f64::from(y));
                        }
                        if !out.is_empty() && vnc_tx.write_all(&out).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "takeover: closing RFB connection");
                        break;
                    }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => break,
            },
        }
    }
    let _ = ws_tx.send(Message::Close(None)).await;
}

/// Parses the RFB client-to-server stream and drops input while the person
/// may not drive the display.
#[derive(Debug, Default)]
struct RfbFilter {
    /// 0 ProtocolVersion, 1 security type, 2 ClientInit, 3 normal messages.
    phase: u8,
    buf: Vec<u8>,
    /// Position of the last pointer event forwarded, until taken.
    pointer: Option<(u16, u16)>,
}

impl RfbFilter {
    /// Bytes to forward and how many input messages among them, or why the
    /// stream must close.
    fn feed(
        &mut self,
        data: &[u8],
        may_input: &dyn Fn() -> bool,
    ) -> Result<(Vec<u8>, usize), String> {
        self.buf.extend_from_slice(data);
        let (mut out, mut inputs, mut at) = (Vec::new(), 0, 0);
        while let Some(len) = self.frame_len(&self.buf[at..])? {
            let frame = &self.buf[at..at + len];
            // Key, pointer, clipboard, QEMU key and desktop resize act on the display.
            let input = self.phase == 3 && matches!(frame[0], 4 | 5 | 6 | 251 | 255);
            if !input || may_input() {
                out.extend_from_slice(frame);
                inputs += usize::from(input);
                if input && frame[0] == 5 {
                    self.pointer = Some((
                        u16::from_be_bytes([frame[2], frame[3]]),
                        u16::from_be_bytes([frame[4], frame[5]]),
                    ));
                }
            }
            self.phase = (self.phase + 1).min(3);
            at += len;
        }
        self.buf.drain(..at);
        Ok((out, inputs))
    }

    /// Length of the complete frame at the start of `b`; `None` until it has arrived.
    fn frame_len(&self, b: &[u8]) -> Result<Option<usize>, String> {
        let Some(&kind) = b.first() else {
            return Ok(None);
        };
        let need = |n: usize| (b.len() >= n).then_some(n);
        let be16 = |i: usize| {
            b.get(i..i + 2)
                .map(|s| usize::from(u16::from_be_bytes([s[0], s[1]])))
        };
        Ok(match (self.phase, kind) {
            (0, _) if b.len() < 12 => None,
            (0, _) if &b[..12] == b"RFB 003.008\n" => Some(12),
            (0, _) => return Err("unsupported RFB version (3.8 required)".into()),
            (1, 1) => Some(1),
            (1, t) => return Err(format!("security type {t} refused (None required)")),
            (2, _) => Some(1),
            (_, 0) => need(20),
            (_, 2) => be16(2).and_then(|n| need(4 + 4 * n)),
            (_, 3) | (_, 150) => need(10),
            (_, 4) => need(8),
            // The marker bit selects the extended (7-byte) pointer event.
            (_, 5) => b
                .get(1)
                .and_then(|m| need(if m & 0x80 != 0 { 7 } else { 6 })),
            (_, 6) => match b
                .get(4..8)
                .map(|s| i32::from_be_bytes([s[0], s[1], s[2], s[3]]).unsigned_abs() as usize)
            {
                Some(n) if n > MAX_CUT_TEXT => {
                    return Err(format!("clipboard of {n} bytes refused"));
                }
                Some(n) => need(8 + n),
                None => None,
            },
            (_, 248) => b.get(8).and_then(|&n| need(9 + usize::from(n))),
            (_, 251) => b.get(6).and_then(|&n| need(8 + 16 * usize::from(n))),
            (_, 255) => match b.get(1) {
                None => None,
                Some(0) => need(12),
                Some(s) => return Err(format!("QEMU message {s} refused")),
            },
            (_, t) => return Err(format!("unknown RFB message type {t}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    const HELLO: &[u8] = b"RFB 003.008\n\x01\x01";
    const KEY: [u8; 8] = [4, 1, 0, 0, 0, 0, 0, 0x61];
    const POINTER: [u8; 6] = [5, 1, 0, 10, 0, 20];
    const UPDATE: [u8; 10] = [3, 1, 0, 0, 0, 0, 5, 0, 3, 32];

    fn stream(extra: &[&[u8]]) -> Vec<u8> {
        let mut v = HELLO.to_vec();
        extra.iter().for_each(|e| v.extend_from_slice(e));
        v
    }

    #[test]
    fn input_is_forwarded_only_when_allowed_even_byte_by_byte() {
        let encodings: &[u8] = &[2, 0, 0, 2, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x21];
        let cut: &[u8] = &[6, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c'];
        let all = stream(&[encodings, &KEY, &POINTER, cut, &UPDATE]);
        let control = stream(&[encodings, &UPDATE]);
        for chunk in [1, 3, all.len()] {
            for (allowed, expected, inputs) in [(false, &control, 0), (true, &all, 3)] {
                let mut f = RfbFilter::default();
                let (mut out, mut n) = (Vec::new(), 0);
                for c in all.chunks(chunk) {
                    let (o, i) = f.feed(c, &|| allowed).unwrap();
                    out.extend(o);
                    n += i;
                }
                assert_eq!(
                    (&out, n),
                    (expected, inputs),
                    "chunk {chunk} allowed {allowed}"
                );
                assert!(f.buf.is_empty());
            }
        }
    }

    #[test]
    fn unknown_messages_and_other_security_close_the_stream() {
        assert!(
            RfbFilter::default()
                .feed(&stream(&[&[7, 0, 0]]), &|| true)
                .is_err()
        );
        assert!(
            RfbFilter::default()
                .feed(b"RFB 003.008\n\x02", &|| true)
                .is_err()
        );
        assert!(
            RfbFilter::default()
                .feed(b"RFB 003.003\n", &|| true)
                .is_err()
        );
        let huge = stream(&[&[6, 0, 0, 0, 0x7f, 0, 0, 0]]);
        assert!(RfbFilter::default().feed(&huge, &|| true).is_err());
        let qemu = stream(&[&[255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 30]]);
        assert_eq!(
            RfbFilter::default().feed(&qemu, &|| false).unwrap(),
            (HELLO.to_vec(), 0)
        );
    }

    async fn http(addr: SocketAddr, request: &str) -> String {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        tcp.read_to_string(&mut out).await.unwrap();
        out
    }

    #[test]
    fn forwarded_pointer_events_report_their_position() {
        let mut f = RfbFilter::default();
        f.feed(&stream(&[&POINTER]), &|| false).unwrap();
        assert_eq!(f.pointer, None, "dropped input moves nothing");
        f.feed(&POINTER, &|| true).unwrap();
        assert_eq!(f.pointer, Some((10, 20)));
    }

    #[tokio::test]
    async fn mounted_page_follows_a_public_url_set_later() {
        use tower::ServiceExt as _;
        let dir = tempfile::tempdir().unwrap();
        let gate = ControlGate::new();
        let config = TakeoverConfig {
            bind: None,
            public_url: Some(Url::parse("https://desk.example/old/").unwrap()),
            token: Some("t0k".into()),
        };
        let t = Takeover::start(config, dir.path().join("vnc.sock"), gate)
            .await
            .unwrap();
        assert_eq!(t.local_addr(), None);
        assert!(
            TakeoverConfig {
                bind: None,
                ..Default::default()
            }
            .public_url
            .is_none()
        );
        t.set_public_url(Url::parse("https://desk.example/v1/takeover/k1").unwrap())
            .unwrap();
        assert_eq!(
            t.url().as_str(),
            "https://desk.example/v1/takeover/k1/?token=t0k"
        );
        assert!(
            t.set_public_url(Url::parse("ftp://desk.example/").unwrap())
                .is_err()
        );
        let call = |uri: &str, origin: Option<&str>| {
            let mut req = axum::http::Request::get(uri).header("host", "127.0.0.1:9");
            if let Some(o) = origin {
                req = req.header("origin", o);
            }
            t.router().oneshot(req.body(Body::empty()).unwrap())
        };
        let page = call("/?token=t0k", None).await.unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let cookie = page.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(
            cookie.contains("Path=/v1/takeover/k1/") && cookie.contains("; Secure"),
            "{cookie}"
        );
        let status = |r: Response| r.status();
        assert_eq!(
            status(
                call("/state?token=t0k", Some("https://desk.example"))
                    .await
                    .unwrap()
            ),
            StatusCode::OK,
            "the public origin may drive the page through a proxy"
        );
        assert_eq!(
            status(
                call("/state?token=t0k", Some("https://other.example"))
                    .await
                    .unwrap()
            ),
            StatusCode::FORBIDDEN
        );
        let router = t.router();
        t.shutdown().await;
        let gone = router
            .oneshot(
                axum::http::Request::get("/state?token=t0k")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn bridge_drops_input_until_a_person_takes_control() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vnc.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let gate = ControlGate::new();
        let config = TakeoverConfig {
            token: Some("t0k".into()),
            ..Default::default()
        };
        let t = Takeover::start(config, sock, gate.clone()).await.unwrap();
        let addr = t.local_addr().unwrap();
        let get = |path: &str, extra: &str| {
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n")
        };
        assert!(
            http(addr, &get("/state", ""))
                .await
                .starts_with("HTTP/1.1 401")
        );
        assert!(
            http(addr, &get("/state?token=nope", ""))
                .await
                .starts_with("HTTP/1.1 403")
        );
        let page = http(addr, &get("/?token=t0k", "")).await;
        let set_cookie = page
            .lines()
            .find_map(|l| l.strip_prefix("set-cookie: "))
            .expect("the page sets its cookie")
            .to_string();
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(set_cookie.contains("=t0k; Path=/; HttpOnly; SameSite=Strict"));
        let cookie_name = set_cookie.split('=').next().unwrap();
        let cookie = format!("Cookie: {cookie_name}=t0k\r\n");
        assert!(
            http(addr, &get("/state", &cookie))
                .await
                .contains(r#"{"holder":"agent"}"#)
        );
        let foreign = format!("{cookie}Origin: http://127.0.0.1:1\r\n");
        assert!(
            http(addr, &get("/state", &foreign))
                .await
                .starts_with("HTTP/1.1 403")
        );
        let long = format!(r#"{{"note":"{}"}}"#, "x".repeat(MAX_NOTE_CHARS + 1));
        let post = format!(
            "POST /release HTTP/1.1\r\nHost: {addr}\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{long}",
            long.len()
        );
        assert!(http(addr, &post).await.starts_with("HTTP/1.1 400"));

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token=t0k"))
            .await
            .unwrap();
        let (mut vnc, _) = listener.accept().await.unwrap();
        vnc.write_all(b"RFB 003.008\n").await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap().into_data().as_ref(),
            b"RFB 003.008\n"
        );
        let send = |b: &[u8]| WsMessage::Binary(Bytes::copy_from_slice(b));
        let mut read = async |n: usize| {
            let mut v = vec![0; n];
            vnc.read_exact(&mut v).await.unwrap();
            v
        };
        ws.send(send(&stream(&[&KEY, &POINTER, &UPDATE])))
            .await
            .unwrap();
        assert_eq!(read(HELLO.len() + UPDATE.len()).await, stream(&[&UPDATE]));
        gate.take(Some("tester".into()));
        ws.send(send(&KEY)).await.unwrap();
        assert_eq!(read(KEY.len()).await, KEY);
        gate.release(None);
        ws.send(send(&[&POINTER[..], &UPDATE].concat()))
            .await
            .unwrap();
        assert_eq!(read(UPDATE.len()).await, UPDATE);
        ws.send(send(&[9])).await.unwrap();
        assert_eq!(
            vnc.read(&mut [0; 1]).await.unwrap(),
            0,
            "unknown type closes the bridge"
        );
        t.shutdown().await;
    }
}
