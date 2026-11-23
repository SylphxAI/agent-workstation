//! End-to-end tests for the compact CLI subcommands and `skills` subcommand.

use std::collections::VecDeque;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};

const TOKEN: &str = "cli-integration-token-0123456789";
const SENTINEL_SECRET: &str = "TOP-SECRET-VALUE-NEVER-ECHO-99182";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-workstation"))
}

fn run_cli(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = bin();
    cmd.env_remove("AGENT_WORKSTATION_URL")
        .env_remove("AGENT_WORKSTATION_TOKEN")
        .env_remove("AGENT_WORKSTATION_SESSION")
        .env_remove("AGENT_WORKSTATION_PRINCIPAL")
        .env_remove("AGENT_WORKSTATION_SESSION_FILE")
        .env_remove("AGENT_WORKSTATION_PROFILE_DIR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.args(args).output().expect("spawn agent-workstation")
}

#[test]
fn skills_subcommand_works_offline_and_stamps_exact_version() {
    let version = env!("CARGO_PKG_VERSION");

    let ver_out = run_cli(&["--version"], &[]);
    assert!(ver_out.status.success());
    let ver_stdout = String::from_utf8(ver_out.stdout).unwrap();
    assert!(ver_stdout.contains(version), "{ver_stdout}");

    let index = run_cli(&["skills"], &[]);
    assert!(
        index.status.success(),
        "{:?}",
        String::from_utf8_lossy(&index.stderr)
    );
    let index_text = String::from_utf8(index.stdout).unwrap();
    assert!(index_text.contains(&format!("v{version}")), "{index_text}");
    assert!(index_text.contains("agent-workstation skills core"));
    assert!(index_text.contains("agent-workstation skills desktop"));
    assert!(index_text.contains("agent-workstation skills takeover"));

    let list = run_cli(&["skills", "--list"], &[]);
    assert!(list.status.success());
    assert_eq!(String::from_utf8(list.stdout).unwrap(), index_text);

    for (topic, expected_cmd) in [
        ("core", "agent-workstation fill-secret"),
        ("desktop", "agent-workstation desktop snapshot"),
        ("takeover", "agent-workstation takeover start"),
    ] {
        let out = run_cli(&["skills", topic], &[]);
        assert!(out.status.success(), "skills {topic} failed");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains(&format!("v{version}")), "{text}");
        assert!(text.contains(expected_cmd), "{text}");
    }
}

#[derive(Default)]
struct MockState {
    sessions: Vec<String>,
    human_in_control: bool,
    recorded: Vec<(String, String, Value)>,
    next_call_error: VecDeque<(StatusCode, Value)>,
}

fn check_auth(headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let expected = format!("Bearer {TOKEN}");
    let ok = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(expected.as_str());
    if ok {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "unauthorized" })),
        ))
    }
}

async fn mock_sessions(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let s = state.lock().unwrap();
    let entries: Vec<Value> = s
        .sessions
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "takeover_url": format!("http://127.0.0.1:9999/?token=tok-{id}"),
                "quiesced": false,
                "in_flight": 0,
            })
        })
        .collect();
    Ok(Json(json!({ "sessions": entries })))
}

async fn mock_create(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let mut s = state.lock().unwrap();
    let id = format!("sess-{}", s.sessions.len() + 1);
    s.sessions.push(id.clone());
    s.recorded.push(("POST".into(), "/sessions".into(), body));
    Ok(Json(json!({
        "id": id,
        "takeover_url": format!("http://127.0.0.1:9999/?token=tok-{id}"),
    })))
}

async fn mock_delete(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let if_agent = uri.query().is_some_and(|q| {
        url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "if_agent" && v == "true")
    });
    let mut s = state.lock().unwrap();
    s.recorded.push((
        "DELETE".into(),
        format!("/sessions/{id}"),
        json!({ "if_agent": if_agent }),
    ));
    if if_agent && s.human_in_control {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "code": "human-in-control",
                "message": "a person is in control of this session"
            })),
        ));
    }
    s.sessions.retain(|x| x != &id);
    Ok(StatusCode::NO_CONTENT)
}

async fn mock_takeover_status(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let s = state.lock().unwrap();
    Ok(Json(json!({
        "holder": if s.human_in_control { "human" } else { "agent" },
        "takeover_url": format!("http://127.0.0.1:9999/?token=tok-{id}"),
        "idle_timeout_ms": 300000,
        "idle_remaining_ms": if s.human_in_control { Value::from(299000) } else { Value::Null },
        "browser_running": true,
    })))
}

async fn mock_takeover_start(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let mut s = state.lock().unwrap();
    s.human_in_control = true;
    s.recorded.push((
        "POST".into(),
        format!("/sessions/{id}/takeover/start"),
        body.clone(),
    ));
    Ok(Json(json!({
        "holder": "human",
        "principal": body["principal"],
        "takeover_url": format!("http://127.0.0.1:9999/?token=tok-{id}"),
        "idle_timeout_ms": 300000,
        "idle_remaining_ms": 300000,
        "browser_running": true,
    })))
}

async fn mock_takeover_end(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let mut s = state.lock().unwrap();
    let released = s.human_in_control;
    s.human_in_control = false;
    s.recorded
        .push(("POST".into(), format!("/sessions/{id}/takeover/end"), body));
    Ok(Json(json!({
        "released": released,
        "status": {
            "holder": "agent",
            "takeover_url": format!("http://127.0.0.1:9999/?token=tok-{id}"),
            "idle_timeout_ms": 300000,
            "idle_remaining_ms": null,
            "browser_running": true,
        }
    })))
}

async fn mock_call(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers)?;
    let mut s = state.lock().unwrap();
    if let Some(err) = s.next_call_error.pop_front() {
        return Err((err.0, Json(err.1)));
    }
    s.recorded
        .push(("POST".into(), format!("/sessions/{id}/call"), body.clone()));
    let op = body["op"].as_str().unwrap_or("");
    let sample_png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nmock");
    let resp = match op {
        "navigate" => json!({ "url": body["url"] }),
        "snapshot" => json!({
            "id": "s1",
            "url": "https://example.com/",
            "text": "button \"Sign in\" [@e1]\ntextbox \"Email\" [@e2]\ntextbox \"Password\" [@e3]",
            "refs": 3,
        }),
        "click" | "fill" | "fill_secret" | "click_at" | "desktop_click_ref" | "type" | "key"
        | "hold_key" | "scroll" => json!({ "ok": true }),
        "eval" => json!({ "value": 42 }),
        "page_screenshot" => json!({
            "width": 1280,
            "height": 800,
            "truncated": false,
            "png_base64": sample_png,
        }),
        "screenshot" => json!({
            "width": 1280,
            "height": 800,
            "png_base64": sample_png,
        }),
        "desktop_snapshot" => json!({
            "id": "d1",
            "text": "window \"Example - Chromium\" app=chromium id=14 rect=0,0 1280x800 focused [@e1]",
            "windows": [{
                "ref": "e1",
                "id": 14,
                "name": "Example - Chromium",
                "app_id": "chromium",
                "pid": 4242,
                "focused": true,
                "rect": {"x": 0, "y": 0, "width": 1280, "height": 800}
            }],
            "refs": 1
        }),
        "desktop_windows" => json!({
            "windows": [{
                "id": 14,
                "name": "Example - Chromium",
                "app_id": "chromium",
                "pid": 4242,
                "focused": true,
                "rect": {"x": 0, "y": 0, "width": 1280, "height": 800}
            }]
        }),
        "desktop_focus" => json!({
            "window": {
                "id": 14,
                "name": "Example - Chromium",
                "app_id": "chromium",
                "pid": 4242,
                "focused": true,
                "rect": {"x": 0, "y": 0, "width": 1280, "height": 800}
            }
        }),
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "code": "invalid-input", "message": format!("unexpected op {other}") }),
                ),
            ));
        }
    };
    Ok(Json(resp))
}

#[tokio::test]
async fn compact_cli_exercises_all_subcommands_against_daemon_wire_protocol() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let app = Router::new()
        .route("/sessions", get(mock_sessions).post(mock_create))
        .route("/sessions/{id}", delete(mock_delete))
        .route("/sessions/{id}/call", post(mock_call))
        .route("/sessions/{id}/takeover", get(mock_takeover_status))
        .route("/sessions/{id}/takeover/start", post(mock_takeover_start))
        .route("/sessions/{id}/takeover/end", post(mock_takeover_end))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("session.json");
    let shot_path = dir.path().join("page.png");
    let desktop_shot_path = dir.path().join("desktop.png");
    let endpoint = format!("http://{addr}");
    let session_file_str = session_file.display().to_string();
    let env = [
        ("AGENT_WORKSTATION_URL", endpoint.as_str()),
        ("AGENT_WORKSTATION_TOKEN", TOKEN),
        ("AGENT_WORKSTATION_SESSION_FILE", session_file_str.as_str()),
        ("AGENT_WORKSTATION_PRINCIPAL", "agent-cli-test"),
    ];

    let invoke = |args: &[&str]| -> Output {
        let args_owned: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let env_owned: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let arg_refs: Vec<&str> = args_owned.iter().map(String::as_str).collect();
        let env_refs: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_cli(&arg_refs, &env_refs)
    };

    // 1. open creates and persists session, then navigates
    let out = tokio::task::spawn_blocking({
        let endpoint = endpoint.clone();
        let session_file_str = session_file_str.clone();
        move || {
            run_cli(
                &["open", "https://example.com/login"],
                &[
                    ("AGENT_WORKSTATION_URL", &endpoint),
                    ("AGENT_WORKSTATION_TOKEN", TOKEN),
                    ("AGENT_WORKSTATION_SESSION_FILE", &session_file_str),
                    ("AGENT_WORKSTATION_PRINCIPAL", "agent-cli-test"),
                ],
            )
        }
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("session=sess-1"), "{stdout}");
    assert!(stdout.contains("https://example.com/login"), "{stdout}");
    assert!(session_file.exists());

    // Helper for running CLI commands off the Tokio worker thread
    let exec = |args: Vec<String>| {
        let endpoint = endpoint.clone();
        let session_file_str = session_file_str.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                run_cli(
                    &refs,
                    &[
                        ("AGENT_WORKSTATION_URL", &endpoint),
                        ("AGENT_WORKSTATION_TOKEN", TOKEN),
                        ("AGENT_WORKSTATION_SESSION_FILE", &session_file_str),
                        ("AGENT_WORKSTATION_PRINCIPAL", "agent-cli-test"),
                    ],
                )
            })
            .await
            .unwrap()
        }
    };
    let _ = invoke;

    // 2. snapshot
    let snap = exec(vec!["snapshot".into()]).await;
    assert!(snap.status.success());
    let snap_out = String::from_utf8(snap.stdout).unwrap();
    assert!(snap_out.contains("# snapshot s1"), "{snap_out}");
    assert!(snap_out.contains("[@e1]"), "{snap_out}");

    // 3. click, fill, fill-secret (never echoes secret)
    assert!(
        exec(vec!["click".into(), "@s1:e1".into()])
            .await
            .status
            .success()
    );
    assert!(
        exec(vec![
            "fill".into(),
            "@e2".into(),
            "alice@example.com".into()
        ])
        .await
        .status
        .success()
    );
    let sec = exec(vec![
        "fill-secret".into(),
        "@e3".into(),
        "vault/account/password".into(),
        "--expect-origin".into(),
        "https://example.com".into(),
    ])
    .await;
    assert!(sec.status.success());
    let sec_out = String::from_utf8(sec.stdout).unwrap();
    let sec_err = String::from_utf8(sec.stderr).unwrap();
    assert!(!sec_out.contains(SENTINEL_SECRET));
    assert!(!sec_err.contains(SENTINEL_SECRET));

    // 4. StaleRef propagates clearly with non-zero exit
    state.lock().unwrap().next_call_error.push_back((
        StatusCode::CONFLICT,
        json!({ "code": "stale-ref", "message": "stale ref: @s0:e1" }),
    ));
    let stale = exec(vec!["click".into(), "@s0:e1".into()]).await;
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale-ref"));

    // 5. eval
    let ev = exec(vec!["eval".into(), "40 + 2".into()]).await;
    assert!(ev.status.success());
    assert_eq!(String::from_utf8(ev.stdout).unwrap().trim(), "42");

    // 6. screenshot writes PNG to disk
    let shot = exec(vec![
        "screenshot".into(),
        shot_path.display().to_string(),
        "--full-page".into(),
    ])
    .await;
    assert!(shot.status.success());
    assert_eq!(std::fs::read(&shot_path).unwrap(), b"\x89PNG\r\n\x1a\nmock");

    // 7. desktop subcommands
    let dsnap = exec(vec!["desktop".into(), "snapshot".into()]).await;
    assert!(dsnap.status.success());
    assert!(String::from_utf8(dsnap.stdout).unwrap().contains("[@e1]"));

    let dwins = exec(vec!["desktop".into(), "windows".into()]).await;
    assert!(dwins.status.success());
    assert!(
        String::from_utf8(dwins.stdout)
            .unwrap()
            .contains("Example - Chromium")
    );

    let dfocus = exec(vec!["desktop".into(), "focus".into(), "@e1".into()]).await;
    assert!(dfocus.status.success());
    assert!(
        String::from_utf8(dfocus.stdout)
            .unwrap()
            .contains("focused 14")
    );

    assert!(
        exec(vec![
            "desktop".into(),
            "click".into(),
            "640".into(),
            "400".into(),
            "--button".into(),
            "right".into(),
            "--modifiers".into(),
            "ctrl".into(),
        ])
        .await
        .status
        .success()
    );
    assert!(
        exec(vec!["desktop".into(), "click".into(), "@e1".into()])
            .await
            .status
            .success()
    );
    assert!(
        exec(vec!["desktop".into(), "type".into(), "hello".into()])
            .await
            .status
            .success()
    );
    assert!(
        exec(vec!["desktop".into(), "key".into(), "ctrl+l".into()])
            .await
            .status
            .success()
    );
    assert!(
        exec(vec![
            "desktop".into(),
            "key".into(),
            "Return".into(),
            "--hold-ms".into(),
            "150".into(),
        ])
        .await
        .status
        .success()
    );
    assert!(
        exec(vec![
            "desktop".into(),
            "scroll".into(),
            "640".into(),
            "400".into(),
            "--dx".into(),
            "0".into(),
            "--dy".into(),
            "-3".into(),
        ])
        .await
        .status
        .success()
    );
    let dshot = exec(vec![
        "desktop".into(),
        "screenshot".into(),
        desktop_shot_path.display().to_string(),
    ])
    .await;
    assert!(dshot.status.success());
    assert_eq!(
        std::fs::read(&desktop_shot_path).unwrap(),
        b"\x89PNG\r\n\x1a\nmock"
    );

    // 8. takeover status / start / atomic close refusal / end / atomic close success
    let t_status = exec(vec!["takeover".into()]).await;
    assert!(t_status.status.success());
    let t_status_out = String::from_utf8(t_status.stdout).unwrap();
    assert!(t_status_out.contains("holder=agent"), "{t_status_out}");
    assert!(t_status_out.contains("takeover_url=http://127.0.0.1:9999/?token=tok-sess-1"));

    let t_start = exec(vec![
        "takeover".into(),
        "start".into(),
        "--label".into(),
        "operator".into(),
    ])
    .await;
    assert!(t_start.status.success());
    assert!(
        String::from_utf8(t_start.stdout)
            .unwrap()
            .contains("holder=human")
    );

    // Atomic close refuses while human holds control, preserving session file
    let refused_close = exec(vec!["close".into()]).await;
    assert!(!refused_close.status.success());
    assert!(String::from_utf8_lossy(&refused_close.stderr).contains("human-in-control"));
    assert!(session_file.exists());

    // Hand back, then atomic close succeeds and cleans session file
    let t_end = exec(vec![
        "takeover".into(),
        "end".into(),
        "--note".into(),
        "done".into(),
    ])
    .await;
    assert!(t_end.status.success());
    assert!(
        String::from_utf8(t_end.stdout)
            .unwrap()
            .contains("released=true")
    );

    let ok_close = exec(vec!["close".into()]).await;
    assert!(
        ok_close.status.success(),
        "{}",
        String::from_utf8_lossy(&ok_close.stderr)
    );
    assert!(!session_file.exists());

    // Verify wire payloads sent to daemon
    let recorded = &state.lock().unwrap().recorded;
    assert!(recorded.iter().any(|(_, p, b)| p == "/sessions/sess-1/call"
        && b["op"] == "fill_secret"
        && b["secret_ref"] == "vault/account/password"
        && b["expect_origin"] == "https://example.com"));
    assert!(recorded.iter().any(|(_, p, b)| p == "/sessions/sess-1/call"
        && b["op"] == "desktop_click_ref"
        && b["ref"] == "@e1"));
    assert!(
        recorded
            .iter()
            .any(|(m, p, b)| m == "DELETE" && p == "/sessions/sess-1" && b["if_agent"] == true)
    );

    server.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn headful_compact_cli_drives_real_daemon_session() {
    use agent_workstation::platform::{DesktopNode, DesktopTree};
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command as AsyncCommand;

    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipping headful compact CLI test; set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }

    // Minimal local HTTP fixture page
    let page = r#"<!doctype html><html><head><title>CLI Fixture</title></head><body>
        <label>Email <input id="email" aria-label="Email"></label>
        <label>Password <input id="pw" type="password" aria-label="Password"></label>
        <button id="go" onclick="document.getElementById('out').textContent='done:'+email.value+':'+pw.value.length">Go</button>
        <p id="out">ready</p>
    </body></html>"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    let fixture_url = format!("http://{fixture_addr}/");
    let fixture_origin = format!("http://{fixture_addr}");

    let dir = tempfile::tempdir().unwrap();
    let secret_script = dir.path().join("secret.sh");
    std::fs::write(
        &secret_script,
        format!("#!/bin/sh\nif [ \"$1\" = \"vault/cli/pw\" ]; then printf '%s\\n' '{SENTINEL_SECRET}'; exit 0; fi\nexit 1\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let browser =
        std::env::var("AGENT_WORKSTATION_BROWSER").unwrap_or_else(|_| "/usr/bin/chromium".into());
    let mut daemon_cmd = AsyncCommand::new(env!("CARGO_BIN_EXE_agent-workstation"));
    if let Ok(args) = std::env::var("AGENT_WORKSTATION_BROWSER_ARGS") {
        for arg in args.split_whitespace() {
            daemon_cmd.args(["--browser-arg", arg]);
        }
    }
    let mut child = daemon_cmd
        .args(["--listen", "127.0.0.1:0", "--state-dir"])
        .arg(dir.path().join("state"))
        .args(["--browser", &browser, "--secret-command"])
        .arg(&secret_script)
        .arg("--")
        .env("AGENT_WORKSTATION_TOKEN", TOKEN)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let addr = line
        .strip_prefix("agent-workstation listening on ")
        .unwrap_or_else(|| panic!("daemon failed to start: {line}"))
        .to_owned();
    let endpoint = format!("http://{addr}");
    let session_file = dir.path().join("session.json").display().to_string();
    let profile_dir = dir.path().join("profile").display().to_string();
    let shot_path = dir.path().join("page.png");

    let exec = |args: Vec<String>| {
        let endpoint = endpoint.clone();
        let session_file = session_file.clone();
        let profile_dir = profile_dir.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                run_cli(
                    &refs,
                    &[
                        ("AGENT_WORKSTATION_URL", &endpoint),
                        ("AGENT_WORKSTATION_TOKEN", TOKEN),
                        ("AGENT_WORKSTATION_SESSION_FILE", &session_file),
                        ("AGENT_WORKSTATION_PROFILE_DIR", &profile_dir),
                    ],
                )
            })
            .await
            .unwrap()
        }
    };

    let open = exec(vec!["open".into(), fixture_url.clone()]).await;
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );

    let snap = exec(vec!["snapshot".into()]).await;
    assert!(
        snap.status.success(),
        "{}",
        String::from_utf8_lossy(&snap.stderr)
    );
    let snap_text = String::from_utf8(snap.stdout).unwrap();
    let find_ref = |needle: &str| -> String {
        let line = snap_text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not in {snap_text}"));
        let suffix = line.rsplit("[@").next().unwrap().trim_end_matches(']');
        format!("@{suffix}")
    };
    let email_ref = find_ref("textbox \"Email\"");
    let pw_ref = find_ref("textbox \"Password\"");
    let go_ref = find_ref("button \"Go\"");

    assert!(
        exec(vec!["fill".into(), email_ref, "cli@example.com".into()])
            .await
            .status
            .success()
    );
    let fill_sec = exec(vec![
        "fill-secret".into(),
        pw_ref,
        "vault/cli/pw".into(),
        "--expect-origin".into(),
        fixture_origin,
    ])
    .await;
    assert!(
        fill_sec.status.success(),
        "{}",
        String::from_utf8_lossy(&fill_sec.stderr)
    );
    assert!(!String::from_utf8_lossy(&fill_sec.stdout).contains(SENTINEL_SECRET));
    assert!(!String::from_utf8_lossy(&fill_sec.stderr).contains(SENTINEL_SECRET));

    assert!(exec(vec!["click".into(), go_ref]).await.status.success());

    let ev = exec(vec![
        "eval".into(),
        "document.getElementById('out').textContent".into(),
    ])
    .await;
    assert!(ev.status.success());
    let ev_text = String::from_utf8(ev.stdout).unwrap();
    assert!(
        ev_text.contains(&format!("done:cli@example.com:{}", SENTINEL_SECRET.len())),
        "{ev_text}"
    );
    assert!(!ev_text.contains(SENTINEL_SECRET));

    let shot = exec(vec!["screenshot".into(), shot_path.display().to_string()]).await;
    assert!(shot.status.success());
    assert!(Path::new(&shot_path).metadata().unwrap().len() > 100);

    // The default daemon path must address the fixture's actual accessibility controls.
    let desktop = |args: &[&str]| {
        let mut cmd = vec!["--json".into(), "desktop".into()];
        cmd.extend(args.iter().map(|arg| (*arg).to_owned()));
        exec(cmd)
    };
    let decode = |out: std::process::Output| -> serde_json::Value {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    fn control_ref<'a>(nodes: &'a [DesktopNode], name: &str, roles: &[&str]) -> Option<&'a str> {
        nodes.iter().find_map(|node| {
            if node.name == name && roles.contains(&node.role.as_str()) {
                Some(node.qualified_ref.as_str())
            } else {
                control_ref(&node.children, name, roles)
            }
        })
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let (go, email) = loop {
        let snap = decode(desktop(&["snapshot", "--max-depth", "40", "--max-nodes", "400"]).await);
        if snap["mode"] == "native_accessibility" {
            let tree: DesktopTree = serde_json::from_value(snap.clone()).unwrap();
            // AT-SPI role names differ between browser/AT-SPI versions.
            if let (Some(go), Some(email)) = (
                control_ref(&tree.roots, "Go", &["button", "push_button"]),
                control_ref(&tree.roots, "Email", &["entry", "text", "text_field"]),
            ) {
                break (go.to_owned(), email.to_owned());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "missing fixture controls (mode {}, fallback {}): {}",
                snap["mode"],
                snap["fallback_reason"],
                snap["text"].as_str().unwrap_or("<no tree>")
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    };
    let focus = decode(desktop(&["focus", &email]).await);
    assert_eq!(focus["mode"], "native_accessibility");
    assert_eq!(focus["target"], email);
    // Chromium exposes native focus but does not implement EditableText.SetTextContents.
    let _ = decode(desktop(&["key", "ctrl+a"]).await);
    let _ = decode(desktop(&["type", "native@example.test"]).await);
    let _ = decode(desktop(&["click", &go]).await);
    let result = exec(vec![
        "eval".into(),
        "document.getElementById('out').textContent".into(),
    ])
    .await;
    assert!(String::from_utf8_lossy(&result.stdout).contains("done:native@example.test:"));

    // Keep another session alive to test that its first snapshot cannot rebind a foreign ref.
    let first: Value = serde_json::from_slice(&std::fs::read(&session_file).unwrap()).unwrap();
    let first_id = first["session_id"].as_str().unwrap();
    let second = decode(
        exec(vec![
            "--json".into(),
            "open".into(),
            fixture_url,
            "--new".into(),
            "--profile-dir".into(),
            dir.path().join("other-profile").display().to_string(),
        ])
        .await,
    );
    let second_id = second["session"].as_str().unwrap();
    assert_ne!(second_id, first_id);
    let shallow = decode(desktop(&["snapshot", "--max-depth", "1", "--max-nodes", "1"]).await);
    assert_eq!(shallow["mode"], "native_accessibility");
    assert_eq!(shallow["truncated"], true);
    assert_eq!(shallow["total_nodes"], 1);
    let reject = |out: std::process::Output| {
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("stale-ref"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    reject(desktop(&["click", &go]).await);
    reject(desktop(&["focus", "@e1"]).await);
    let app = shallow["roots"][0]["qualified_ref"].as_str().unwrap();
    let subtree = decode(desktop(&["snapshot", "--root", app, "--max-depth", "2"]).await);
    assert_eq!(subtree["mode"], "native_accessibility");
    assert!(subtree["total_nodes"].as_u64().unwrap() > 1);
    reject(desktop(&["snapshot", "--root", app]).await);

    // Remove discovery for only this session, leaving the other session and
    // compositor live. A native ref must not rebind to a fallback window.
    let bus = dir.path().join("state").join(second_id).join("dbus");
    let hidden = bus.with_file_name("hidden-bus");
    std::fs::rename(&bus, &hidden).unwrap();
    let fallback = decode(desktop(&["snapshot"]).await);
    assert_eq!(fallback["mode"], "compositor");
    assert_eq!(fallback["fallback_reason"], "accessibility_unavailable");
    let window = fallback["windows"][0]["ref"].as_str().unwrap();
    assert!(window.starts_with("@win-"));
    let old_native = subtree["roots"][0]["qualified_ref"].as_str().unwrap();
    reject(desktop(&["click", old_native]).await);
    let focused = decode(desktop(&["focus", window]).await);
    assert_eq!(focused["mode"], "compositor");
    std::fs::rename(&hidden, &bus).unwrap();
    let _ = decode(exec(vec!["--json".into(), "close".into()]).await);
    std::fs::write(&session_file, serde_json::to_vec(&first).unwrap()).unwrap();

    // Takeover start -> atomic close refused -> takeover end -> atomic close succeeds
    let t_start = exec(vec!["takeover".into(), "start".into()]).await;
    assert!(t_start.status.success());
    let t_start_text = String::from_utf8(t_start.stdout).unwrap();
    assert!(t_start_text.contains("holder=human"), "{t_start_text}");
    assert!(
        t_start_text.contains("takeover_url=http://"),
        "{t_start_text}"
    );

    let refused = exec(vec!["close".into()]).await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("human-in-control"));

    let t_end = exec(vec!["takeover".into(), "end".into()]).await;
    assert!(t_end.status.success());

    let closed = exec(vec!["close".into()]).await;
    assert!(
        closed.status.success(),
        "{}",
        String::from_utf8_lossy(&closed.stderr)
    );
    let _ = child.kill().await;
}
