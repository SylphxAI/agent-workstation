//! A synthetic localhost passkey must survive a fresh Chromium process and profile.
#![cfg(target_os = "linux")]

use std::path::Path;

use agent_workstation::browser::{Browser, Web};
use agent_workstation::cdp::Cdp;
use agent_workstation::{BrowserConfig, Fingerprint, VirtualAuthenticatorOptions};
use axum::{Router, response::Html, routing::get};
use serde_json::{Value, json};

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
