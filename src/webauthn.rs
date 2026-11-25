//! Types for Chromium's CDP virtual WebAuthn authenticator.
//!
//! These are software authenticators, not hardware or OS-backed passkeys.
//! Private keys cross the trusted DevTools connection; callers are responsible
//! for protecting that connection and any credential material they retain.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// An opaque Chromium virtual authenticator identifier for a page target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VirtualAuthenticatorId(String);

impl VirtualAuthenticatorId {
    /// The opaque identifier Chromium assigned.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Virtual authenticator protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthenticatorProtocol {
    /// CTAP2, including support for discoverable credentials.
    #[default]
    Ctap2,
    /// Legacy FIDO U2F.
    U2f,
}

/// Transport reported by the virtual authenticator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthenticatorTransport {
    /// Platform authenticator.
    #[default]
    Internal,
    /// USB security key.
    Usb,
    /// Near-field communication.
    Nfc,
    /// Bluetooth Low Energy.
    Ble,
    /// Cloud-assisted Bluetooth Low Energy.
    Cable,
}

/// Chromium virtual authenticator options. Defaults to a CTAP2 platform
/// authenticator with resident keys, simulated presence and verification, and
/// credentials marked as backup-eligible and backed up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualAuthenticatorOptions {
    /// The authenticator protocol.
    pub protocol: AuthenticatorProtocol,
    /// The authenticator transport.
    pub transport: AuthenticatorTransport,
    /// Support discoverable (resident) credentials.
    pub has_resident_key: bool,
    /// Support user verification.
    pub has_user_verification: bool,
    /// Whether user verification succeeds.
    pub is_user_verified: bool,
    /// Automatically satisfy user presence checks.
    pub automatic_presence_simulation: bool,
    /// Default backup eligibility of newly created credentials.
    pub default_backup_eligibility: bool,
    /// Default backup state of newly created credentials.
    pub default_backup_state: bool,
}

impl Default for VirtualAuthenticatorOptions {
    fn default() -> Self {
        Self {
            protocol: AuthenticatorProtocol::default(),
            transport: AuthenticatorTransport::default(),
            has_resident_key: true,
            has_user_verification: true,
            is_user_verified: true,
            automatic_presence_simulation: true,
            default_backup_eligibility: true,
            default_backup_state: true,
        }
    }
}

/// Raw credential material backed by a virtual authenticator.
///
/// This type deliberately does not implement serde serialization. The CDP
/// wire representation is private; callers persisting credentials should use
/// their own explicit, protected storage format.
pub struct PasskeyCredential {
    /// Opaque credential ID bytes.
    pub credential_id: Vec<u8>,
    /// Whether this credential is discoverable without an allow-list.
    pub is_resident_credential: bool,
    /// Relying party ID, not a URL or origin. Exports without one are rejected.
    pub rp_id: String,
    /// Secret ECDSA private key in DER PKCS#8 format.
    pub private_key_pkcs8: Vec<u8>,
    /// Opaque user ID, if supplied by the relying party.
    pub user_handle: Option<Vec<u8>>,
    /// Signature counter. The CDP sentinel for a disabled counter (-1) is not supported.
    pub sign_count: u32,
    /// Backup eligibility, if specified.
    pub backup_eligibility: Option<bool>,
    /// Backup state, if specified.
    pub backup_state: Option<bool>,
}

impl fmt::Debug for PasskeyCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeyCredential")
            .field("private_key_pkcs8", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireCredential {
    credential_id: String,
    is_resident_credential: bool,
    rp_id: String,
    private_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_handle: Option<String>,
    sign_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_eligibility: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_state: Option<bool>,
}

impl From<&PasskeyCredential> for WireCredential {
    fn from(credential: &PasskeyCredential) -> Self {
        Self {
            credential_id: STANDARD.encode(&credential.credential_id),
            is_resident_credential: credential.is_resident_credential,
            rp_id: credential.rp_id.clone(),
            private_key: STANDARD.encode(&credential.private_key_pkcs8),
            user_handle: credential
                .user_handle
                .as_ref()
                .map(|id| STANDARD.encode(id)),
            sign_count: credential.sign_count,
            backup_eligibility: credential.backup_eligibility,
            backup_state: credential.backup_state,
        }
    }
}

impl TryFrom<WireCredential> for PasskeyCredential {
    type Error = Error;

    fn try_from(wire: WireCredential) -> Result<Self> {
        fn decode(value: String, field: &str) -> Result<Vec<u8>> {
            STANDARD
                .decode(value)
                .map_err(|_| Error::protocol(format!("WebAuthn: invalid base64 in {field}")))
        }

        Ok(Self {
            credential_id: decode(wire.credential_id, "credentialId")?,
            is_resident_credential: wire.is_resident_credential,
            rp_id: wire.rp_id,
            private_key_pkcs8: decode(wire.private_key, "privateKey")?,
            user_handle: wire
                .user_handle
                .map(|v| decode(v, "userHandle"))
                .transpose()?,
            sign_count: wire.sign_count,
            backup_eligibility: wire.backup_eligibility,
            backup_state: wire.backup_state,
        })
    }
}

#[cfg(test)]
mod tests;
