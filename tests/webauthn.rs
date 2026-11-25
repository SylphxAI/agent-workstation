//! A synthetic localhost passkey must survive a fresh Chromium process and profile.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use agent_workstation::browser::{Browser, Web};
use agent_workstation::cdp::Cdp;
use agent_workstation::{
    BrowserConfig, Error, Fingerprint, VirtualAuthenticatorOptions, WebAuthnCeremonyLeaseConfig,
    WebAuthnCeremonyOutcome, WebAuthnCeremonyScope, WebAuthnDenyReason, WebAuthnDomError,
    WebAuthnOperation, WebAuthnProxyRequest, WebAuthnProxyResponse, webauthn_signer,
};
use axum::{Router, response::Html, routing::get};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn launch(config: &BrowserConfig, profile: &Path) -> Browser {
    let log = profile.join("chrome.log");
    Browser::launch(
        config,
        &[],
        profile,
        &Fingerprint::generate("webauthn", &Default::default()),
        None,
        &log,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}: {}", std::fs::read_to_string(log).unwrap_or_default()))
}

#[tokio::test]
async fn real_headless_chromium_webauthn() {
    let Ok(binary) = std::env::var("AGENT_WORKSTATION_WEBAUTHN_BROWSER") else {
        eprintln!("set AGENT_WORKSTATION_WEBAUTHN_BROWSER to run the real Chromium WebAuthn test");
        return;
    };
    let mut config = BrowserConfig::new(binary);
    config.extra_args = vec!["--headless".into(), "--disable-gpu".into()];
    // Explicit opt-out only for disposable environments without usable Chromium sandboxing.
    if std::env::var_os("AGENT_WORKSTATION_WEBAUTHN_NO_SANDBOX").is_some() {
        config.extra_args.push("--no-sandbox".into());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                get(|| async { Html(include_str!("fixtures/webauthn.html")) }),
            ),
        )
        .await
        .unwrap();
    });
    let options = VirtualAuthenticatorOptions::default();
    let source = tempfile::tempdir().unwrap();
    let browser = launch(&config, source.path()).await;
    let mut browser_cdp = Cdp::connect(browser.endpoint()).await.unwrap();
    assert!(
        browser_cdp
            .call("WebAuthn.enable", json!({}))
            .await
            .is_err()
    );
    drop(browser_cdp);
    let mut web = Web::connect(browser.endpoint()).await.unwrap();
    web.navigate(&origin).await.unwrap();
    assert_eq!(web.eval("isSecureContext").await.unwrap(), true);
    let original_tab = web
        .tabs()
        .await
        .unwrap()
        .into_iter()
        .find(|tab| tab.active)
        .unwrap()
        .id;
    web.enable_webauthn().await.unwrap();
    let id = web.add_virtual_authenticator(&options).await.unwrap();
    let registration = web.eval("register()").await.unwrap();
    let flags = registration["flags"].as_u64().unwrap();
    // UP, UV, BE and BS must all be set on the default synced passkey.
    assert_eq!(flags & 0x1d, 0x1d);
    let mut captured = web.get_credentials(&id).await.unwrap();
    assert_eq!(captured.len(), 1);
    let credential = captured.pop().unwrap();
    assert!(credential.is_resident_credential);
    assert_eq!(credential.rp_id, "localhost");
    assert!(!credential.credential_id.is_empty());
    assert_eq!(json!(credential.credential_id), registration["rawId"]);
    assert_eq!(json!(credential.user_handle), registration["userHandle"]);
    assert!(!credential.private_key_pkcs8.is_empty());
    assert_eq!(json!(credential.sign_count), registration["counter"]);
    assert_eq!(credential.backup_eligibility, Some(flags & 0x08 != 0));
    assert_eq!(credential.backup_state, Some(flags & 0x10 != 0));

    let other_tab = web.new_tab("about:blank").await.unwrap();
    web.navigate(&origin).await.unwrap();
    assert!(web.get_credentials(&id).await.is_err());
    assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
    web.select_tab(&original_tab).await.unwrap();
    let remaining = web.get_credentials(&id).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].credential_id, credential.credential_id);
    web.close_tab(&other_tab).await.unwrap();
    drop(web);
    browser.shutdown().await;

    let destination = tempfile::tempdir().unwrap();
    let browser = launch(&config, destination.path()).await;
    let mut web = Web::connect(browser.endpoint()).await.unwrap();
    web.navigate(&origin).await.unwrap();
    web.enable_webauthn().await.unwrap();
    let id = web.add_virtual_authenticator(&options).await.unwrap();
    assert!(web.get_credentials(&id).await.unwrap().is_empty());
    web.add_credential(&id, &credential).await.unwrap();
    let assertion: Value = web
        .eval(&format!("authenticate({})", registration["publicKey"]))
        .await
        .unwrap();
    assert_eq!(assertion["rawId"], registration["rawId"]);
    assert_eq!(assertion["userHandle"], registration["userHandle"]);
    assert_eq!(assertion["clientDataMatches"], true);
    assert_eq!(assertion["rpHashMatches"], true);
    assert_eq!(assertion["signatureValid"], true);
    assert_eq!(assertion["flags"].as_u64().unwrap() & 0x1d, flags & 0x1d);
    assert_eq!(assertion["counter"], credential.sign_count + 1);
    let stored = web.get_credentials(&id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].sign_count, credential.sign_count + 1);

    web.clear_credentials(&id).await.unwrap();
    assert!(web.get_credentials(&id).await.unwrap().is_empty());
    assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
    web.add_credential(&id, &credential).await.unwrap();
    web.remove_credential(&id, &credential.credential_id)
        .await
        .unwrap();
    assert!(web.get_credentials(&id).await.unwrap().is_empty());
    assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
    web.add_credential(&id, &credential).await.unwrap();
    web.remove_virtual_authenticator(&id).await.unwrap();
    assert!(web.get_credentials(&id).await.is_err());
    let id = web.add_virtual_authenticator(&options).await.unwrap();
    web.add_credential(&id, &credential).await.unwrap();
    web.disable_webauthn().await.unwrap();
    assert!(web.get_credentials(&id).await.is_err());
    assert!(web.add_virtual_authenticator(&options).await.is_err());
    web.enable_webauthn().await.unwrap();
    let id = web.add_virtual_authenticator(&options).await.unwrap();
    assert!(web.get_credentials(&id).await.unwrap().is_empty());
    assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
    browser.shutdown().await;
    server.abort();
}

/// Pure host-side ES256 keypair and WebAuthn response builder used in tests;
/// private key material stays outside Chromium at all times.
struct ExternalTestPasskey {
    private_pem: String,
    spki_bytes: Vec<u8>,
    cred_id_b64url: String,
    user_handle_b64url: String,
    sign_count: AtomicU32,
}

impl ExternalTestPasskey {
    fn generate() -> Self {
        let out = std::process::Command::new("node")
            .args([
                "-e",
                r#"
const crypto = require('node:crypto');
const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
const spki = Array.from(publicKey.export({ type: 'spki', format: 'der' }));
const pem = privateKey.export({ type: 'pkcs8', format: 'pem' });
const credId = crypto.randomBytes(32).toString('base64url');
const userHandle = Buffer.from([0, 1, 127, 128, 255]).toString('base64url');
process.stdout.write(JSON.stringify({ pem, spki, credId, userHandle }));
"#,
            ])
            .output()
            .expect("node must be installed for external passkey test fixture");
        assert!(
            out.status.success(),
            "node keygen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        Self {
            private_pem: v["pem"].as_str().unwrap().to_owned(),
            spki_bytes: v["spki"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_u64().unwrap() as u8)
                .collect(),
            cred_id_b64url: v["credId"].as_str().unwrap().to_owned(),
            user_handle_b64url: v["userHandle"].as_str().unwrap().to_owned(),
            sign_count: AtomicU32::new(0),
        }
    }

    fn sign(&self, request: &WebAuthnProxyRequest) -> WebAuthnProxyResponse {
        let op = match request.operation {
            WebAuthnOperation::Create => "create",
            WebAuthnOperation::Get => "get",
        };
        let counter = match request.operation {
            WebAuthnOperation::Create => 0,
            WebAuthnOperation::Get => self.sign_count.fetch_add(1, Ordering::SeqCst) + 1,
        };
        let input = json!({
            "op": op,
            "pem": self.private_pem,
            "credId": self.cred_id_b64url,
            "userHandle": self.user_handle_b64url,
            "rpId": request.rp_id,
            "origin": request.origin,
            "counter": counter,
            "details": request.request_details_json,
        });
        let script = r#"
const crypto = require('node:crypto');
const inp = JSON.parse(process.argv[1]);
const details = JSON.parse(inp.details);
const priv = crypto.createPrivateKey(inp.pem);
const pub = crypto.createPublicKey(priv);
const rpHash = crypto.createHash('sha256').update(inp.rpId).digest();

if (inp.op === 'create') {
  const jwk = pub.export({ format: 'jwk' });
  const x = Buffer.from(jwk.x, 'base64url');
  const y = Buffer.from(jwk.y, 'base64url');
  const spki = pub.export({ type: 'spki', format: 'der' });
  const credId = Buffer.from(inp.credId, 'base64url');
  // Minimal canonical CBOR encoding for COSE EC2 P-256 key: {1:2, 3:-7, -1:1, -2:x, -3:y}
  const bstr = (buf) => Buffer.concat([buf.length < 24 ? Buffer.from([0x40 | buf.length]) : Buffer.from([0x58, buf.length]), buf]);
  const coseKey = Buffer.concat([
    Buffer.from([0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21]),
    bstr(x),
    Buffer.from([0x22]),
    bstr(y),
  ]);
  const aaguid = Buffer.alloc(16, 0);
  const idLen = Buffer.alloc(2); idLen.writeUInt16BE(credId.length, 0);
  const counterBuf = Buffer.alloc(4); counterBuf.writeUInt32BE(inp.counter, 0);
  const flags = Buffer.from([0x5d]); // UP | UV | BE | BS | AT
  const authData = Buffer.concat([rpHash, flags, counterBuf, aaguid, idLen, credId, coseKey]);
  // CBOR map {"fmt": "none", "attStmt": {}, "authData": authData}
  const cborBytes = (buf) => {
    if (buf.length < 24) return Buffer.concat([Buffer.from([0x40 | buf.length]), buf]);
    if (buf.length < 256) return Buffer.concat([Buffer.from([0x58, buf.length]), buf]);
    const h = Buffer.alloc(3); h[0] = 0x59; h.writeUInt16BE(buf.length, 1);
    return Buffer.concat([h, buf]);
  };
  const cborText = (s) => { const b = Buffer.from(s, 'utf8'); return Buffer.concat([Buffer.from([0x60 | b.length]), b]); };
  const attestationObject = Buffer.concat([
    Buffer.from([0xa3]),
    cborText('fmt'), cborText('none'),
    cborText('attStmt'), Buffer.from([0xa0]),
    cborText('authData'), cborBytes(authData),
  ]);
  const clientDataJSON = Buffer.from(JSON.stringify({
    type: 'webauthn.create', challenge: details.challenge, origin: inp.origin, crossOrigin: false,
  }));
  process.stdout.write(JSON.stringify({
    id: inp.credId, rawId: inp.credId, type: 'public-key', authenticatorAttachment: 'platform',
    response: {
      attestationObject: attestationObject.toString('base64url'),
      authenticatorData: authData.toString('base64url'),
      clientDataJSON: clientDataJSON.toString('base64url'),
      publicKey: spki.toString('base64url'),
      publicKeyAlgorithm: -7,
      transports: ['internal'],
    },
    clientExtensionResults: {},
  }));
} else {
  const clientDataJSON = Buffer.from(JSON.stringify({
    type: 'webauthn.get', challenge: details.challenge, origin: inp.origin, crossOrigin: false,
  }));
  const counterBuf = Buffer.alloc(4); counterBuf.writeUInt32BE(inp.counter, 0);
  const flags = Buffer.from([0x1d]); // UP | UV | BE | BS
  const authData = Buffer.concat([rpHash, flags, counterBuf]);
  const clientHash = crypto.createHash('sha256').update(clientDataJSON).digest();
  const signature = crypto.sign('sha256', Buffer.concat([authData, clientHash]), priv);
  process.stdout.write(JSON.stringify({
    id: inp.credId, rawId: inp.credId, type: 'public-key', authenticatorAttachment: 'platform',
    response: {
      clientDataJSON: clientDataJSON.toString('base64url'),
      authenticatorData: authData.toString('base64url'),
      signature: signature.toString('base64url'),
      userHandle: inp.userHandle,
    },
    clientExtensionResults: {},
  }));
}
"#;
        let out = std::process::Command::new("node")
            .args(["-e", script, &input.to_string()])
            .output()
            .expect("node signer failed to spawn");
        if !out.status.success() {
            return WebAuthnProxyResponse::Error(WebAuthnDomError::not_allowed(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        WebAuthnProxyResponse::Success {
            response_json: String::from_utf8(out.stdout).unwrap(),
        }
    }
}

async fn unpacked_extension_count(endpoint: &str) -> usize {
    let mut cdp = Cdp::connect(endpoint).await.unwrap();
    let res = cdp
        .call_in(
            None,
            "Extensions.getExtensions",
            json!({}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    res["extensions"].as_array().map_or(0, Vec::len)
}

async fn active_bridge_port(endpoint: &str) -> u16 {
    let mut cdp = Cdp::connect(endpoint).await.unwrap();
    let res = cdp
        .call_in(
            None,
            "Extensions.getExtensions",
            json!({}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let path = res["extensions"][0]["path"].as_str().unwrap();
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(path).join("manifest.json")).unwrap(),
    )
    .unwrap();
    let host_perm = manifest["host_permissions"][0].as_str().unwrap();
    let parsed = url::Url::parse(host_perm.trim_end_matches('*')).unwrap();
    parsed.port().unwrap()
}

async fn raw_http_post(port: u16, headers: &[(&str, &str)], body: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req = format!(
        "POST /request HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    let text = String::from_utf8_lossy(&resp);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn real_chromium_origin_bound_webauthn_ceremony_lease() {
    let Ok(binary) = std::env::var("AGENT_WORKSTATION_WEBAUTHN_BROWSER") else {
        eprintln!("set AGENT_WORKSTATION_WEBAUTHN_BROWSER to run the real Chromium WebAuthn test");
        return;
    };

    // Origin A and Origin B share rpId = "localhost" while having distinct origins.
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let origin_a = format!("http://localhost:{port_a}");

    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    let origin_b = format!("http://localhost:{port_b}");

    let origin_b_for_a = origin_b.clone();
    let server_a = tokio::spawn(async move {
        let iframe_page = format!(
            r#"<!doctype html><meta charset="utf-8"><script>
window.iframeResult = new Promise(r => window.addEventListener('message', e => r(e.data)));
</script><iframe id="child" src="{origin_b_for_a}/iframe" allow="publickey-credentials-get *; publickey-credentials-create *"></iframe>"#
        );
        axum::serve(
            listener_a,
            Router::new()
                .route(
                    "/",
                    get(|| async { Html(include_str!("fixtures/webauthn.html")) }),
                )
                .route(
                    "/with-iframe",
                    get(move || async move { Html(iframe_page) }),
                ),
        )
        .await
        .unwrap();
    });

    let server_b = tokio::spawn(async move {
        let base_html = include_str!("fixtures/webauthn.html");
        let iframe_html = format!(
            r#"{base_html}<script>
window.addEventListener('message', async (e) => {{
  if (e.data === 'run-get') {{
    try {{
      const cred = await navigator.credentials.get({{ publicKey: assertionOptions(), signal: AbortSignal.timeout(3000) }});
      parent.postMessage({{ ok: true, id: cred.id }}, '*');
    }} catch (err) {{
      parent.postMessage({{ ok: false, name: err.name, message: err.message }}, '*');
    }}
  }}
}});
</script>"#
        );
        let bfcache_html = format!(
            r#"{base_html}<script>
window.bfcacheResult = null;
window.addEventListener('pageshow', async (ev) => {{
  if (ev.persisted) {{
    try {{
      const cred = await navigator.credentials.get({{ publicKey: assertionOptions(), signal: AbortSignal.timeout(3000) }});
      window.bfcacheResult = {{ ok: true, persisted: true, id: cred.id }};
    }} catch (err) {{
      window.bfcacheResult = {{ ok: false, persisted: true, name: err.name, message: err.message }};
    }}
  }}
}});
</script>"#
        );
        let sw_register_html = r#"<!doctype html><script>
window.swReady = navigator.serviceWorker.register('/sw.js').then(async r => { await navigator.serviceWorker.ready; return r.scope; });
</script>"#;
        let sw_js = r#"
self.addEventListener('install', e => e.waitUntil(self.skipWaiting()));
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));
self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (url.pathname === '/sw-steal') {
    e.respondWith(new Response(`<!doctype html><script>
      window.swAttempt = (async () => {
        try {
          const cred = await navigator.credentials.get({
            publicKey: { rpId: 'localhost', userVerification: 'required', challenge: crypto.getRandomValues(new Uint8Array(32)) },
            signal: AbortSignal.timeout(3000),
          });
          return { ok: true, id: cred.id };
        } catch (err) {
          return { ok: false, name: err.name, message: err.message };
        }
      })();
    </script>`, { headers: { 'Content-Type': 'text/html; charset=utf-8' } }));
  }
});
"#;
        axum::serve(
            listener_b,
            Router::new()
                .route("/", get(move || async move { Html(base_html) }))
                .route("/iframe", get(move || async move { Html(iframe_html) }))
                .route("/bfcache", get(move || async move { Html(bfcache_html) }))
                .route(
                    "/sw-register",
                    get(move || async move { Html(sw_register_html) }),
                )
                .route(
                    "/sw.js",
                    get(move || async move {
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                            sw_js,
                        )
                    }),
                ),
        )
        .await
        .unwrap();
    });

    let mut config = BrowserConfig::new(&binary);
    config.extra_args = vec!["--headless".into(), "--disable-gpu".into()];
    if std::env::var_os("AGENT_WORKSTATION_WEBAUTHN_NO_SANDBOX").is_some() {
        config.extra_args.push("--no-sandbox".into());
    }
    config.enable_webauthn_proxy();

    let profile = tempfile::tempdir().unwrap();
    let browser = launch(&config, profile.path()).await;
    let endpoint = browser.endpoint().to_owned();
    let mut web = Web::connect(&endpoint).await.unwrap();
    let passkey = Arc::new(ExternalTestPasskey::generate());

    // ---- POSITIVE: create() + two get() calls with monotonic signCount, shadowing prevention, and clean detach ----
    web.navigate(&origin_a).await.unwrap();
    // Enable a virtual authenticator first; arm_webauthn_ceremony must disable it so it cannot shadow the proxy.
    web.enable_webauthn().await.unwrap();
    let _stale_virt = web
        .add_virtual_authenticator(&VirtualAuthenticatorOptions::default())
        .await
        .unwrap();

    let create_calls = Arc::new(AtomicUsize::new(0));
    let lease = web
        .arm_webauthn_ceremony(
            WebAuthnCeremonyLeaseConfig::new(&origin_a).with_scope(WebAuthnCeremonyScope::Create),
            {
                let pk = passkey.clone();
                let calls = create_calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            },
        )
        .await
        .unwrap();

    // Virtual WebAuthn must be refused while a proxy ceremony lease is active.
    assert!(matches!(
        web.enable_webauthn().await,
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        web.add_virtual_authenticator(&VirtualAuthenticatorOptions::default())
            .await,
        Err(Error::Refused(_))
    ));

    // Negative 3a: live unauthenticated bridge requests (no token, wrong token, web Origin) are refused with 403 and 0 signer calls.
    let bridge_port = active_bridge_port(&endpoint).await;
    let probe_body = json!({
        "kind": "create",
        "requestId": 999,
        "requestDetailsJson": json!({
            "rp": { "id": "localhost" },
            "extensions": { "remoteDesktopClientOverride": { "origin": origin_a, "sameOriginWithAncestors": true } }
        }).to_string()
    })
    .to_string();
    assert_eq!(
        raw_http_post(
            bridge_port,
            &[(
                "Origin",
                "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )],
            &probe_body,
        )
        .await,
        403
    );
    assert_eq!(
        raw_http_post(
            bridge_port,
            &[
                (
                    "Origin",
                    "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                ("X-WebAuthn-Bridge-Token", "deadbeef"),
            ],
            &probe_body,
        )
        .await,
        403
    );
    assert_eq!(
        raw_http_post(
            bridge_port,
            &[
                ("Origin", &origin_a),
                ("X-WebAuthn-Bridge-Token", "deadbeef")
            ],
            &probe_body,
        )
        .await,
        403
    );
    assert_eq!(create_calls.load(Ordering::SeqCst), 0);

    // Legitimate create() on originA succeeds via the external host signer!
    let registration = web.eval("register()").await.unwrap();
    assert_eq!(registration["publicKey"], json!(passkey.spki_bytes));
    assert_eq!(registration["flags"].as_u64().unwrap() & 0x1d, 0x1d);
    assert_eq!(registration["counter"], 0);
    assert_eq!(create_calls.load(Ordering::SeqCst), 1);
    let outcome = lease.finish().await.unwrap();
    assert_eq!(
        outcome,
        WebAuthnCeremonyOutcome::Completed {
            operation: WebAuthnOperation::Create,
            origin: origin_a.clone(),
            same_origin_with_ancestors: true,
        }
    );
    assert_eq!(unpacked_extension_count(&endpoint).await, 0);

    // Two sequential get() leases verify monotonic signCount (1 -> 2) and full WebCrypto ES256 verification.
    for expected_counter in [1u64, 2u64] {
        let get_calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(
                WebAuthnCeremonyLeaseConfig::new(&origin_a).with_scope(WebAuthnCeremonyScope::Get),
                {
                    let pk = passkey.clone();
                    let calls = get_calls.clone();
                    webauthn_signer(move |req| {
                        let pk = pk.clone();
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            pk.sign(&req)
                        }
                    })
                },
            )
            .await
            .unwrap();
        let assertion = web
            .eval(&format!("authenticate({})", registration["publicKey"]))
            .await
            .unwrap();
        assert_eq!(assertion["clientDataMatches"], true);
        assert_eq!(assertion["rpHashMatches"], true);
        assert_eq!(assertion["signatureValid"], true);
        assert_eq!(assertion["counter"], expected_counter);
        assert_eq!(get_calls.load(Ordering::SeqCst), 1);
        let outcome = lease.finish().await.unwrap();
        assert_eq!(
            outcome,
            WebAuthnCeremonyOutcome::Completed {
                operation: WebAuthnOperation::Get,
                origin: origin_a.clone(),
                same_origin_with_ancestors: true,
            }
        );
        assert_eq!(unpacked_extension_count(&endpoint).await, 0);
    }

    // Outside armed leases, proxy is detached so conditional mediation is not rejected immediately with NotAllowedError.
    let cond = web
        .eval(
            r#"(async () => {
              const t0 = performance.now();
              try {
                await navigator.credentials.get({
                  mediation: 'conditional',
                  publicKey: assertionOptions(),
                  signal: AbortSignal.timeout(250),
                });
                return { status: 'resolved', elapsed: performance.now() - t0 };
              } catch (e) {
                return { status: e.name, elapsed: performance.now() - t0 };
              }
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        cond["status"], "TimeoutError",
        "conditional mediation should remain pending until abort after proxy detach: {cond}"
    );

    // ---- NEGATIVE 1A: Same-RP different-origin direct top navigation (originA -> originB) ----
    {
        web.navigate(&origin_a).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(WebAuthnCeremonyLeaseConfig::new(&origin_a), {
                let pk = passkey.clone();
                let calls = calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            })
            .await
            .unwrap();
        web.navigate(&origin_b).await.unwrap();
        let denied = web
            .eval(
                r#"(async () => {
                  try {
                    await navigator.credentials.get({ publicKey: assertionOptions(), signal: AbortSignal.timeout(3000) });
                    return { ok: true };
                  } catch (e) {
                    return { ok: false, name: e.name };
                  }
                })()"#,
            )
            .await
            .unwrap();
        assert_eq!(denied["ok"], false, "{denied}");
        assert_eq!(denied["name"], "NotAllowedError", "{denied}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease.finish().await.unwrap(),
            WebAuthnCeremonyOutcome::Denied {
                reason: WebAuthnDenyReason::OriginMismatch,
            }
        );
        assert_eq!(unpacked_extension_count(&endpoint).await, 0);
    }

    // ---- NEGATIVE 1B: BFCache history.back() to originB (persisted == true) ----
    {
        web.navigate(&origin_a).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(WebAuthnCeremonyLeaseConfig::new(&origin_a), {
                let pk = passkey.clone();
                let calls = calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            })
            .await
            .unwrap();
        // Visit originB/bfcache and navigate to originA while the lease is armed so
        // originB enters BFCache without extension-install eviction, then history.back().
        web.navigate(&format!("{origin_b}/bfcache")).await.unwrap();
        web.eval(&format!("location.href = {}; true", json!(origin_a)))
            .await
            .unwrap();
        let nav_deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while !web.current_url().await.unwrap().starts_with(&origin_a) {
            assert!(tokio::time::Instant::now() < nav_deadline);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        web.eval("history.back(); true").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        let bf_res = loop {
            if let Ok(v) = web.eval("window.bfcacheResult").await
                && !v.is_null()
            {
                break v;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "bfcache pageshow did not settle"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        assert_eq!(bf_res["persisted"], true, "{bf_res}");
        assert_eq!(bf_res["ok"], false, "{bf_res}");
        assert_eq!(bf_res["name"], "NotAllowedError", "{bf_res}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease.finish().await.unwrap(),
            WebAuthnCeremonyOutcome::Denied {
                reason: WebAuthnDenyReason::OriginMismatch,
            }
        );
    }

    // ---- NEGATIVE 1C: Service Worker navigation to originB ----
    {
        web.navigate(&format!("{origin_b}/sw-register"))
            .await
            .unwrap();
        let _ = web.eval("window.swReady").await.unwrap();
        web.navigate(&origin_a).await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(WebAuthnCeremonyLeaseConfig::new(&origin_a), {
                let pk = passkey.clone();
                let calls = calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            })
            .await
            .unwrap();
        web.navigate(&format!("{origin_b}/sw-steal")).await.unwrap();
        let sw_res = web.eval("window.swAttempt").await.unwrap();
        assert_eq!(sw_res["ok"], false, "{sw_res}");
        assert_eq!(sw_res["name"], "NotAllowedError", "{sw_res}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease.finish().await.unwrap(),
            WebAuthnCeremonyOutcome::Denied {
                reason: WebAuthnDenyReason::OriginMismatch,
            }
        );
    }

    // ---- NEGATIVE 2: Delegated cross-origin descendant iframe (both origin mismatch AND CrossOriginAncestor!) ----
    for (leased_origin, expected_reason) in [
        (&origin_a, WebAuthnDenyReason::OriginMismatch),
        (&origin_b, WebAuthnDenyReason::CrossOriginAncestor),
    ] {
        web.navigate(&format!("{origin_a}/with-iframe"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(WebAuthnCeremonyLeaseConfig::new(leased_origin), {
                let pk = passkey.clone();
                let calls = calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            })
            .await
            .unwrap();
        let iframe_res = web
            .eval(
                r#"(async () => {
                  document.getElementById('child').contentWindow.postMessage('run-get', '*');
                  return await window.iframeResult;
                })()"#,
            )
            .await
            .unwrap();
        assert_eq!(iframe_res["ok"], false, "{iframe_res}");
        assert_eq!(iframe_res["name"], "NotAllowedError", "{iframe_res}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease.finish().await.unwrap(),
            WebAuthnCeremonyOutcome::Denied {
                reason: expected_reason,
            }
        );
    }

    // ---- NEGATIVE 3B: Lease timeout tears down fail-closed ----
    {
        web.navigate(&origin_a).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(
                WebAuthnCeremonyLeaseConfig::new(&origin_a)
                    .with_timeout(Duration::from_millis(250)),
                {
                    let pk = passkey.clone();
                    let calls = calls.clone();
                    webauthn_signer(move |req| {
                        let pk = pk.clone();
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            pk.sign(&req)
                        }
                    })
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease.finish().await.unwrap(),
            WebAuthnCeremonyOutcome::Expired
        );
        assert_eq!(unpacked_extension_count(&endpoint).await, 0);
    }

    // ---- NEGATIVE 3C: Synchronous Drop without finish() fails closed immediately ----
    {
        web.navigate(&origin_a).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = web
            .arm_webauthn_ceremony(WebAuthnCeremonyLeaseConfig::new(&origin_a), {
                let pk = passkey.clone();
                let calls = calls.clone();
                webauthn_signer(move |req| {
                    let pk = pk.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        pk.sign(&req)
                    }
                })
            })
            .await
            .unwrap();
        drop(lease);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(web.eval("assertNoCredential()").await.unwrap(), true);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Wait briefly for Drop's async uninstall task to complete.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while unpacked_extension_count(&endpoint).await != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "Drop did not uninstall proxy extension"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    browser.shutdown().await;
    server_a.abort();
    server_b.abort();
}
