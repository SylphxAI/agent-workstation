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
