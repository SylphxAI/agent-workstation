//! Types for Chromium WebAuthn automation: software virtual authenticators and
//! origin-bound external-signer ceremony leases.
//!
//! Virtual authenticator commands (`WebAuthn.*`) attach to Chromium's
//! `FrameTreeNode` (inherited by descendant frames and preserved across
//! same-tab navigations) and pass raw private keys over the trusted DevTools
//! connection.
//!
//! When exact-origin, top-document enforcement without placing private keys in
//! Chromium is required, use [`crate::browser::Web::arm_webauthn_ceremony`] (or
//! [`crate::Agent::arm_webauthn_ceremony`]), which bridges Chromium's
//! `chrome.webAuthenticationProxy` API to an out-of-browser [`WebAuthnSigner`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// Chromium flag required for CDP `Extensions.loadUnpacked` / `Extensions.uninstall`
/// used by origin-bound WebAuthn ceremony leases.
pub const WEBAUTHN_PROXY_EXTENSION_FLAG: &str = "--enable-unsafe-extension-debugging";

/// Default bounded TTL for an origin-bound WebAuthn ceremony lease.
pub const DEFAULT_WEBAUTHN_CEREMONY_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum allowed TTL for an origin-bound WebAuthn ceremony lease.
pub const MAX_WEBAUTHN_CEREMONY_TIMEOUT: Duration = Duration::from_secs(120);

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

/// WebAuthn ceremony operation intercepted by the proxy bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebAuthnOperation {
    /// `navigator.credentials.create({ publicKey })`.
    Create,
    /// `navigator.credentials.get({ publicKey })`.
    Get,
}

/// Which WebAuthn operation kind a single-use ceremony lease permits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebAuthnCeremonyScope {
    /// Allow only `navigator.credentials.create()`.
    Create,
    /// Allow only `navigator.credentials.get()`.
    Get,
    /// Allow either `create()` or `get()` (still strictly single-use).
    #[default]
    Any,
}

impl WebAuthnCeremonyScope {
    pub(crate) fn allows(self, operation: WebAuthnOperation) -> bool {
        matches!(
            (self, operation),
            (Self::Any, _)
                | (Self::Create, WebAuthnOperation::Create)
                | (Self::Get, WebAuthnOperation::Get)
        )
    }
}

/// Configuration for a single-use, exact-origin WebAuthn ceremony lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebAuthnCeremonyLeaseConfig {
    /// Exact canonical security origin permitted to invoke the host signer
    /// (for example `https://app.example.com` or `http://localhost:8080`).
    pub allowed_origin: String,
    /// Permitted WebAuthn operation kind for this single-use lease.
    pub scope: WebAuthnCeremonyScope,
    /// Require browser-verified `sameOriginWithAncestors == true` (blocks
    /// cross-origin descendant iframes even with `allow="publickey-credentials-*"`).
    /// Defaults to `true`.
    pub require_same_origin_with_ancestors: bool,
    /// Bounded TTL for the lease. Must be non-zero and at most
    /// [`MAX_WEBAUTHN_CEREMONY_TIMEOUT`].
    pub timeout: Duration,
    /// Result reported to `PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable()`
    /// while this lease is armed. Defaults to `true`.
    pub report_uvpaa: bool,
}

impl WebAuthnCeremonyLeaseConfig {
    /// Create a strict top-document, single-use lease for `allowed_origin`.
    pub fn new(allowed_origin: impl Into<String>) -> Self {
        Self {
            allowed_origin: allowed_origin.into(),
            scope: WebAuthnCeremonyScope::Any,
            require_same_origin_with_ancestors: true,
            timeout: DEFAULT_WEBAUTHN_CEREMONY_TIMEOUT,
            report_uvpaa: true,
        }
    }

    /// Restrict this lease to a specific operation (`Create`, `Get`, or `Any`).
    pub fn with_scope(mut self, scope: WebAuthnCeremonyScope) -> Self {
        self.scope = scope;
        self
    }

    /// Set the bounded lease TTL.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure whether cross-origin ancestors are refused (default `true`).
    pub fn with_require_same_origin_with_ancestors(mut self, require: bool) -> Self {
        self.require_same_origin_with_ancestors = require;
        self
    }

    /// Configure the value reported for `isUserVerifyingPlatformAuthenticatorAvailable()`.
    pub fn with_report_uvpaa(mut self, report_uvpaa: bool) -> Self {
        self.report_uvpaa = report_uvpaa;
        self
    }

    /// Validate configuration and return the canonical security origin string.
    pub(crate) fn validate(&self) -> Result<String> {
        if self.timeout.is_zero() || self.timeout > MAX_WEBAUTHN_CEREMONY_TIMEOUT {
            return Err(Error::InvalidInput(format!(
                "WebAuthn ceremony timeout must be between 1 ms and {} s",
                MAX_WEBAUTHN_CEREMONY_TIMEOUT.as_secs()
            )));
        }
        canonical_webauthn_origin(&self.allowed_origin)
    }
}

/// Validate that `raw` is an exact, canonical trustworthy WebAuthn origin
/// (`https://host[:port]` or `http://localhost[:port]` / `http://*.localhost[:port]`),
/// with no credentials, path, query, or fragment.
pub(crate) fn canonical_webauthn_origin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed != raw {
        return Err(Error::InvalidInput(
            "WebAuthn allowed_origin must be a canonical origin without surrounding whitespace"
                .into(),
        ));
    }
    let parsed = url::Url::parse(raw)
        .map_err(|_| Error::InvalidInput("WebAuthn allowed_origin is not a valid URL".into()))?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || (parsed.path() != "/" && !parsed.path().is_empty())
        || raw.ends_with('/')
    {
        return Err(Error::InvalidInput(
            "WebAuthn allowed_origin must be an exact origin without path, query, fragment, or userinfo"
                .into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidInput("WebAuthn allowed_origin must have a host".into()))?;
    match parsed.scheme() {
        "https" => {}
        "http" if host == "localhost" || host.ends_with(".localhost") => {}
        _ => {
            return Err(Error::InvalidInput(
                "WebAuthn allowed_origin must use https or http://localhost".into(),
            ));
        }
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return Err(Error::InvalidInput(
            "WebAuthn allowed_origin must not be opaque".into(),
        ));
    }
    if origin != raw {
        return Err(Error::InvalidInput(format!(
            "WebAuthn allowed_origin must be canonical (expected {origin:?})"
        )));
    }
    Ok(origin)
}

/// A WebAuthn ceremony request verified by Chromium's browser process and
/// admitted by the lease's exact-origin and ancestor guards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebAuthnProxyRequest {
    /// Whether the page invoked `create()` or `get()`.
    pub operation: WebAuthnOperation,
    /// Unforgeable calling origin stamped by Chromium's browser process.
    pub origin: String,
    /// Browser-verified `sameOriginWithAncestors` flag.
    pub same_origin_with_ancestors: bool,
    /// Relying party ID from the WebAuthn request options.
    pub rp_id: String,
    /// Raw WebAuthn JSON options serialized by Chromium `webAuthenticationProxy`.
    pub request_details_json: String,
}

/// A WebAuthn `DOMException` returned to the calling page when a ceremony is
/// denied or fails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WebAuthnDomError {
    /// DOMException name (for example `"NotAllowedError"`).
    pub name: String,
    /// Human-readable error message.
    pub message: String,
}

impl WebAuthnDomError {
    /// Construct a `NotAllowedError` DOMException.
    pub fn not_allowed(message: impl Into<String>) -> Self {
        Self {
            name: "NotAllowedError".into(),
            message: message.into(),
        }
    }
}

/// Decision returned by an out-of-browser [`WebAuthnSigner`].
#[derive(Clone, PartialEq, Eq)]
pub enum WebAuthnProxyResponse {
    /// Serialized `PublicKeyCredential.toJSON()` payload to deliver to the page.
    Success {
        /// JSON string compatible with Chromium `completeCreateRequest` / `completeGetRequest`.
        response_json: String,
    },
    /// DOMException to reject the page's WebAuthn promise with.
    Error(WebAuthnDomError),
}

impl fmt::Debug for WebAuthnProxyResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success { .. } => f
                .debug_struct("Success")
                .field("response_json", &"[redacted]")
                .finish(),
            Self::Error(error) => f.debug_tuple("Error").field(error).finish(),
        }
    }
}

/// Why a ceremony request was atomically denied before reaching the host signer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebAuthnDenyReason {
    /// Browser-stamped origin did not equal the lease's `allowed_origin`.
    OriginMismatch,
    /// Calling frame had a cross-origin ancestor (`sameOriginWithAncestors == false`).
    CrossOriginAncestor,
    /// Browser-stamped `extensions.remoteDesktopClientOverride` metadata was missing or malformed.
    MissingOriginMetadata,
    /// Request operation (`create` vs `get`) was outside the lease's `scope`.
    ScopeMismatch,
    /// Request JSON or required `rpId`/`rp.id` field was malformed.
    MalformedRequest,
    /// Lease TTL elapsed before dispatch.
    Expired,
    /// Single-use lease already consumed by an earlier request.
    AlreadyUsed,
    /// Lease was cancelled, dropped, or superseded by a control change.
    Cancelled,
}

/// Observable terminal or in-progress outcome of an origin-bound ceremony lease.
/// Never carries private key material or raw signed assertion payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebAuthnCeremonyOutcome {
    /// Lease is still armed and has not received a ceremony request.
    Pending,
    /// Signer was invoked once for a verified request and returned a response.
    Completed {
        /// Ceremony operation that completed.
        operation: WebAuthnOperation,
        /// Verified browser-process origin that matched the lease.
        origin: String,
        /// Verified browser-process `sameOriginWithAncestors` flag.
        same_origin_with_ancestors: bool,
    },
    /// Signer was invoked once for a verified request and returned a DOMException.
    SignerError {
        /// Ceremony operation that was attempted.
        operation: WebAuthnOperation,
        /// Verified browser-process origin that matched the lease.
        origin: String,
        /// DOMException name returned by the signer.
        error_name: String,
    },
    /// Request was atomically refused before invoking the signer.
    Denied {
        /// Exact policy or structural guard that refused the request.
        reason: WebAuthnDenyReason,
    },
    /// Lease expired before a ceremony request arrived.
    Expired,
    /// Lease was revoked, dropped, or interrupted by a session control change.
    Cancelled,
}

/// Out-of-browser host signer for an origin-bound WebAuthn ceremony lease.
///
/// Implementations hold credential custody outside Chromium and are invoked
/// only after the bridge authenticates the extension, enforces single-use and
/// deadline bounds, and verifies Chromium's browser-process origin and
/// ancestor metadata.
#[async_trait]
pub trait WebAuthnSigner: Send + Sync {
    /// Handle one origin-verified WebAuthn ceremony request.
    async fn handle(&self, request: WebAuthnProxyRequest) -> WebAuthnProxyResponse;
}

#[async_trait]
impl<T: WebAuthnSigner + ?Sized> WebAuthnSigner for Arc<T> {
    async fn handle(&self, request: WebAuthnProxyRequest) -> WebAuthnProxyResponse {
        (**self).handle(request).await
    }
}

struct FnWebAuthnSigner<F>(F);

#[async_trait]
impl<F, Fut> WebAuthnSigner for FnWebAuthnSigner<F>
where
    F: Fn(WebAuthnProxyRequest) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = WebAuthnProxyResponse> + Send,
{
    async fn handle(&self, request: WebAuthnProxyRequest) -> WebAuthnProxyResponse {
        (self.0)(request).await
    }
}

/// Wrap an async closure as a [`WebAuthnSigner`].
pub fn webauthn_signer<F, Fut>(f: F) -> impl WebAuthnSigner
where
    F: Fn(WebAuthnProxyRequest) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = WebAuthnProxyResponse> + Send,
{
    FnWebAuthnSigner(f)
}

/// Validate browser-stamped `requestDetailsJson` against the lease contract
/// before any signer invocation.
pub(crate) fn validate_proxy_request(
    operation: WebAuthnOperation,
    request_details_json: String,
    allowed_origin: &str,
    scope: WebAuthnCeremonyScope,
    require_same_origin_with_ancestors: bool,
) -> std::result::Result<WebAuthnProxyRequest, WebAuthnDenyReason> {
    if !scope.allows(operation) {
        return Err(WebAuthnDenyReason::ScopeMismatch);
    }
    let parsed: Value = serde_json::from_str(&request_details_json)
        .map_err(|_| WebAuthnDenyReason::MalformedRequest)?;
    let override_obj = parsed
        .get("extensions")
        .and_then(|e| e.get("remoteDesktopClientOverride"))
        .and_then(Value::as_object)
        .ok_or(WebAuthnDenyReason::MissingOriginMetadata)?;
    let origin = override_obj
        .get("origin")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(WebAuthnDenyReason::MissingOriginMetadata)?;
    let same_origin_with_ancestors = override_obj
        .get("sameOriginWithAncestors")
        .and_then(Value::as_bool)
        .ok_or(WebAuthnDenyReason::MissingOriginMetadata)?;

    if origin != allowed_origin {
        return Err(WebAuthnDenyReason::OriginMismatch);
    }
    if require_same_origin_with_ancestors && !same_origin_with_ancestors {
        return Err(WebAuthnDenyReason::CrossOriginAncestor);
    }

    let rp_id = match operation {
        WebAuthnOperation::Create => parsed
            .get("rp")
            .and_then(|rp| rp.get("id"))
            .and_then(Value::as_str),
        WebAuthnOperation::Get => parsed.get("rpId").and_then(Value::as_str),
    }
    .filter(|s| !s.is_empty())
    .ok_or(WebAuthnDenyReason::MalformedRequest)?
    .to_owned();

    Ok(WebAuthnProxyRequest {
        operation,
        origin: origin.to_owned(),
        same_origin_with_ancestors,
        rp_id,
        request_details_json,
    })
}

#[cfg(test)]
mod tests;
