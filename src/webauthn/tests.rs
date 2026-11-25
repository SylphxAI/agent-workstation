use serde_json::json;

use super::*;

#[test]
fn options_match_cdp_wire_names_and_defaults() {
    assert_eq!(
        serde_json::to_value(VirtualAuthenticatorOptions::default()).unwrap(),
        json!({
            "protocol": "ctap2", "transport": "internal", "hasResidentKey": true,
            "hasUserVerification": true, "isUserVerified": true,
            "automaticPresenceSimulation": true, "defaultBackupEligibility": true,
            "defaultBackupState": true,
        })
    );
    assert_eq!(
        serde_json::to_value(AuthenticatorProtocol::U2f).unwrap(),
        "u2f"
    );
    for (transport, expected) in [
        (AuthenticatorTransport::Usb, "usb"),
        (AuthenticatorTransport::Nfc, "nfc"),
        (AuthenticatorTransport::Ble, "ble"),
        (AuthenticatorTransport::Cable, "cable"),
    ] {
        assert_eq!(serde_json::to_value(transport).unwrap(), expected);
    }
}

#[test]
fn credential_uses_padded_standard_base64_and_preserves_optional_flags() {
    let expected = json!({
        "credentialId": "+/8=", "isResidentCredential": true, "rpId": "localhost",
        "privateKey": "AQIDBA==", "userHandle": "/w==", "signCount": 42,
        "backupEligibility": true, "backupState": false,
    });
    let wire: WireCredential = serde_json::from_value(expected.clone()).unwrap();
    let mut credential = PasskeyCredential::try_from(wire).unwrap();
    assert_eq!(credential.credential_id, [251, 255]);
    assert!(credential.is_resident_credential);
    assert_eq!(credential.rp_id, "localhost");
    assert!(credential.private_key_pkcs8 == [1, 2, 3, 4]);
    assert_eq!(credential.user_handle.as_deref(), Some(&[255][..]));
    assert_eq!(credential.sign_count, 42);
    assert_eq!(credential.backup_eligibility, Some(true));
    assert_eq!(credential.backup_state, Some(false));
    assert_eq!(
        serde_json::to_value(WireCredential::from(&credential)).unwrap(),
        expected
    );

    credential.user_handle = None;
    credential.backup_eligibility = None;
    credential.backup_state = None;
    let wire = serde_json::to_value(WireCredential::from(&credential)).unwrap();
    for field in ["userHandle", "backupEligibility", "backupState"] {
        assert!(wire.get(field).is_none());
    }
    let decoded =
        PasskeyCredential::try_from(serde_json::from_value::<WireCredential>(wire).unwrap())
            .unwrap();
    assert!(
        decoded.user_handle.is_none()
            && decoded.backup_eligibility.is_none()
            && decoded.backup_state.is_none()
    );
}

#[test]
fn debug_and_invalid_key_diagnostics_are_redacted() {
    let wire: WireCredential = serde_json::from_value(json!({
        "credentialId": "+/8=", "isResidentCredential": false, "rpId": "localhost",
        "privateKey": "AQIDBA==", "signCount": 0,
    }))
    .unwrap();
    let credential = PasskeyCredential::try_from(WireCredential {
        private_key: wire.private_key.clone(),
        ..wire
    })
    .unwrap();
    for diagnostic in [format!("{credential:?}"), format!("{credential:#?}")] {
        assert!(diagnostic.contains("[redacted]"));
        assert!(!diagnostic.contains("AQIDBA==") && !diagnostic.contains("[1, 2, 3, 4]"));
    }

    let malformed = "key-with-invalid-base64!";
    let mut wire = WireCredential::from(&credential);
    wire.private_key = malformed.into();
    let error = PasskeyCredential::try_from(wire).unwrap_err();
    assert_eq!(
        error.to_string(),
        "browser protocol: WebAuthn: invalid base64 in privateKey"
    );
    assert!(!format!("{error:?}").contains(malformed));
}

#[test]
fn canonical_origin_and_lease_config_validation() {
    for valid in [
        "https://app.example.com",
        "https://app.example.com:8443",
        "http://localhost",
        "http://localhost:8080",
        "http://sub.localhost:3000",
    ] {
        let cfg = WebAuthnCeremonyLeaseConfig::new(valid);
        assert_eq!(cfg.validate().unwrap(), valid);
    }

    for invalid in [
        "",
        " https://app.example.com",
        "https://app.example.com/",
        "https://app.example.com/login",
        "https://app.example.com?a=1",
        "https://app.example.com#frag",
        "https://user:pass@app.example.com",
        "https://APP.example.com",
        "https://app.example.com:443",
        "http://localhost:80",
        "http://app.example.com",
        "file:///tmp/test.html",
        "null",
    ] {
        let cfg = WebAuthnCeremonyLeaseConfig::new(invalid);
        assert!(
            matches!(cfg.validate(), Err(Error::InvalidInput(_))),
            "expected rejection for {invalid:?}"
        );
    }

    assert!(matches!(
        WebAuthnCeremonyLeaseConfig::new("https://app.example.com")
            .with_timeout(Duration::ZERO)
            .validate(),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        WebAuthnCeremonyLeaseConfig::new("https://app.example.com")
            .with_timeout(MAX_WEBAUTHN_CEREMONY_TIMEOUT + Duration::from_secs(1))
            .validate(),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn validate_proxy_request_enforces_exact_origin_ancestors_scope_and_rp_id() {
    let get_json = |origin: &str, same_origin: bool, rp_id: &str| {
        json!({
            "rpId": rp_id,
            "challenge": "AAAA",
            "extensions": {
                "remoteDesktopClientOverride": {
                    "origin": origin,
                    "sameOriginWithAncestors": same_origin,
                }
            }
        })
        .to_string()
    };
    let create_json = |origin: &str, same_origin: bool, rp_id: &str| {
        json!({
            "rp": { "id": rp_id, "name": "Example" },
            "challenge": "AAAA",
            "extensions": {
                "remoteDesktopClientOverride": {
                    "origin": origin,
                    "sameOriginWithAncestors": same_origin,
                }
            }
        })
        .to_string()
    };

    let req = validate_proxy_request(
        WebAuthnOperation::Get,
        get_json("https://app.example.com", true, "example.com"),
        "https://app.example.com",
        WebAuthnCeremonyScope::Get,
        true,
    )
    .unwrap();
    assert_eq!(req.operation, WebAuthnOperation::Get);
    assert_eq!(req.origin, "https://app.example.com");
    assert!(req.same_origin_with_ancestors);
    assert_eq!(req.rp_id, "example.com");

    let req = validate_proxy_request(
        WebAuthnOperation::Create,
        create_json("https://app.example.com", true, "example.com"),
        "https://app.example.com",
        WebAuthnCeremonyScope::Create,
        true,
    )
    .unwrap();
    assert_eq!(req.operation, WebAuthnOperation::Create);
    assert_eq!(req.rp_id, "example.com");

    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            get_json("https://other.example.com", true, "example.com"),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::OriginMismatch
    );
    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            get_json("https://app.example.com", false, "example.com"),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::CrossOriginAncestor
    );
    assert!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            get_json("https://app.example.com", false, "example.com"),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            false,
        )
        .is_ok()
    );
    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Create,
            create_json("https://app.example.com", true, "example.com"),
            "https://app.example.com",
            WebAuthnCeremonyScope::Get,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::ScopeMismatch
    );
    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            json!({ "rpId": "example.com" }).to_string(),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::MissingOriginMetadata
    );
    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            get_json("https://app.example.com", true, ""),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::MalformedRequest
    );
    assert_eq!(
        validate_proxy_request(
            WebAuthnOperation::Get,
            "{not-json".into(),
            "https://app.example.com",
            WebAuthnCeremonyScope::Any,
            true,
        )
        .unwrap_err(),
        WebAuthnDenyReason::MalformedRequest
    );
}

#[test]
fn proxy_response_debug_redacts_response_json() {
    let secret_payload = r#"{"signature":"synthetic-signature-bytes"}"#;
    let resp = WebAuthnProxyResponse::Success {
        response_json: secret_payload.into(),
    };
    let rendered = format!("{resp:?}");
    assert!(rendered.contains("[redacted]"));
    assert!(!rendered.contains("synthetic-signature-bytes"));
}
