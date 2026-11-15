//! Exercise the compiled daemon without a compositor or browser.
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
const TOKEN: &str = "daemon-test-token-0123456789";
struct Daemon {
    child: Child,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-workstation"))
            .args(["--listen", "127.0.0.1:0", "--state-dir"])
            .arg(dir.path())
            .args(["--browser", "/unused/browser"])
            .env("AGENT_WORKSTATION_TOKEN", TOKEN)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let addr = line
            .strip_prefix("agent-workstation listening on ")
            .unwrap_or_else(|| panic!("startup failed: {line}"))
            .parse()
            .unwrap();
        Self {
            child,
            addr,
            _dir: dir,
        }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> (u16, Value) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = TcpStream::connect(self.addr).await.unwrap();
            let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
            let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
            let value = if body.is_empty() { Value::Null } else { serde_json::from_str(body).unwrap() };
            (status, value)
        }).await.expect("HTTP response deadline")
    }
    async fn auth(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        self.request(method, path, body, Some(TOKEN)).await
    }
    // 100 Continue proves admission ran and extraction is waiting for the body.
    async fn stalled_create(&self) -> TcpStream {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(format!("POST /sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = [0; 25];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&response, b"HTTP/1.1 100 Continue\r\n\r\n");
        stream
    }
}
#[tokio::test]
async fn authentication_precedes_body_and_admission() {
    let daemon = Daemon::start().await;
    for (method, path) in [
        ("GET", "/healthz"),
        ("GET", "/sessions"),
        ("POST", "/sessions"),
        ("POST", "/quiesce"),
        ("POST", "/resume"),
        ("POST", "/sessions/missing/quiesce"),
        ("POST", "/sessions/missing/resume"),
        ("POST", "/sessions/missing/call"),
        ("DELETE", "/sessions/missing"),
    ] {
        for token in [None, Some("incorrect-token-000000")] {
            assert_eq!(
                daemon.request(method, path, "{", token).await.0,
                401,
                "{path}"
            );
        }
    }
    assert_eq!(
        daemon.auth("GET", "/healthz", "").await,
        (200, json!({"status":"ok","sessions":0,"quiesced":false}))
    );
    assert_eq!(
        daemon.auth("GET", "/sessions", "").await,
        (200, json!({"sessions":[]}))
    );
}
#[tokio::test]
async fn quiesce_fences_creation_and_calls_until_resume() {
    let daemon = Daemon::start().await;
    for body in ["", "{}", r#"{"timeout_ms":0}"#, r#"{"timeout_ms":99999}"#] {
        let (status, value) = daemon.auth("POST", "/quiesce", body).await;
        assert_eq!(status, 200);
        assert_eq!(value["quiesced"], true);
        assert!(value["drained_ms"].is_u64());
        assert_eq!(daemon.auth("GET", "/healthz", "").await.1["quiesced"], true);
        for path in ["/sessions", "/sessions/missing/call"] {
            assert_eq!(
                daemon.auth("POST", path, "{").await,
                (
                    409,
                    json!({"code":"quiesced","message":"session is quiesced for snapshot"})
                )
            );
        }
        assert_eq!(
            daemon.auth("POST", "/resume", "{}").await,
            (200, json!({"quiesced":false}))
        );
        // Validation restores admission without launching a browser.
        assert_eq!(
            daemon
                .auth(
                    "POST",
                    "/sessions",
                    r#"{"profile":"","profile_dir":"/unused"}"#
                )
                .await
                .0,
            400
        );
        assert_eq!(
            daemon.auth("POST", "/sessions/missing/call", "{}").await.0,
            404
        );
    }
    for path in ["/sessions/missing/quiesce", "/sessions/missing/resume"] {
        assert_eq!(daemon.auth("POST", path, "").await.0, 404);
    }
    assert_eq!(daemon.auth("POST", "/resume", "").await.0, 200);
    assert_eq!(daemon.auth("POST", "/quiesce", "{").await.0, 400);
    assert_eq!(
        daemon
            .auth("POST", "/resume", r#"{"unexpected":true}"#)
            .await
            .0,
        400
    );
}
#[tokio::test]
async fn timeout_retains_fence_and_disconnect_releases_admission() {
    let daemon = Daemon::start().await;
    let stalled = daemon.stalled_create().await;
    let start = Instant::now();
    assert_eq!(
        daemon
            .auth("POST", "/quiesce", r#"{"timeout_ms":10}"#)
            .await,
        (
            409,
            json!({"code":"quiesce_timeout","message":"in-flight calls did not drain within budget"})
        )
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "unbounded quiesce"
    );
    assert_eq!(daemon.auth("GET", "/healthz", "").await.1["quiesced"], true);
    assert_eq!(
        daemon.auth("POST", "/sessions", "{}").await.1["code"],
        "quiesced"
    );
    drop(stalled);
    assert_eq!(
        daemon
            .auth("POST", "/quiesce", r#"{"timeout_ms":300}"#)
            .await
            .0,
        200
    );
    assert_eq!(daemon.auth("POST", "/resume", "").await.0, 200);
}
#[tokio::test]
async fn completing_invalid_upload_notifies_drain_waiter() {
    let daemon = Daemon::start().await;
    let mut stalled = daemon.stalled_create().await;
    let quiesce = daemon.auth("POST", "/quiesce", r#"{"timeout_ms":300}"#);
    let complete = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while daemon.auth("GET", "/healthz", "").await.1["quiesced"] != true {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        stalled.write_all(&[b'x'; 100]).await.unwrap();
    };
    let (result, ()) = tokio::join!(quiesce, complete);
    assert_eq!(result.0, 200);
}
#[tokio::test]
async fn resume_interrupts_wait_without_claiming_snapshot_safety() {
    let daemon = Daemon::start().await;
    let _stalled = daemon.stalled_create().await;
    let quiesce = daemon.auth("POST", "/quiesce", r#"{"timeout_ms":300}"#);
    let resume = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while daemon.auth("GET", "/healthz", "").await.1["quiesced"] != true {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(daemon.auth("POST", "/resume", "").await.0, 200);
    };
    let (result, ()) = tokio::join!(quiesce, resume);
    assert_eq!(result.0, 409);
    assert_eq!(result.1["code"], "quiesce_cancelled");
    assert_eq!(
        daemon.auth("GET", "/healthz", "").await.1["quiesced"],
        false
    );
}
