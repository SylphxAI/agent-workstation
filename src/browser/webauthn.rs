//! Typed WebAuthn commands on the active page's attached CDP session.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Web;
use crate::webauthn::WireCredential;
use crate::{
    Error, PasskeyCredential, Result, VirtualAuthenticatorId, VirtualAuthenticatorOptions,
};

impl Web {
    /// Enable virtual WebAuthn on the active page, suppressing Chromium's native UI.
    pub async fn enable_webauthn(&mut self) -> Result<()> {
        self.webauthn_call("WebAuthn.enable", json!({ "enableUI": false }))
            .await?;
        Ok(())
    }

    /// Disable virtual WebAuthn and discard that page's virtual authenticators.
    pub async fn disable_webauthn(&mut self) -> Result<()> {
        self.webauthn_call("WebAuthn.disable", json!({})).await?;
        Ok(())
    }

    /// Add a virtual authenticator to the active page after enabling WebAuthn.
    /// Keep the same page selected when using its ID or running a ceremony.
    pub async fn add_virtual_authenticator(
        &mut self,
        options: &VirtualAuthenticatorOptions,
    ) -> Result<VirtualAuthenticatorId> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Added {
            authenticator_id: VirtualAuthenticatorId,
        }
        let result = self
            .webauthn_call(
                "WebAuthn.addVirtualAuthenticator",
                json!({ "options": options }),
            )
            .await?;
        let added: Added = serde_json::from_value(result)
            .map_err(|_| Error::protocol("WebAuthn.addVirtualAuthenticator: invalid response"))?;
        Ok(added.authenticator_id)
    }

    /// Remove a virtual authenticator and its in-memory credentials.
    pub async fn remove_virtual_authenticator(
        &mut self,
        id: &VirtualAuthenticatorId,
    ) -> Result<()> {
        self.webauthn_call(
            "WebAuthn.removeVirtualAuthenticator",
            json!({ "authenticatorId": id }),
        )
        .await?;
        Ok(())
    }

    /// Import a credential into an authenticator on the active page.
    pub async fn add_credential(
        &mut self,
        id: &VirtualAuthenticatorId,
        credential: &PasskeyCredential,
    ) -> Result<()> {
        self.webauthn_call(
            "WebAuthn.addCredential",
            json!({ "authenticatorId": id, "credential": WireCredential::from(credential) }),
        )
        .await?;
        Ok(())
    }

    /// Export credentials, including secret private keys, from an authenticator
    /// on the active page. Keep the returned values out of logs and tool output.
    pub async fn get_credentials(
        &mut self,
        id: &VirtualAuthenticatorId,
    ) -> Result<Vec<PasskeyCredential>> {
        #[derive(Deserialize)]
        struct Credentials {
            credentials: Vec<WireCredential>,
        }
        let result = self
            .webauthn_call("WebAuthn.getCredentials", json!({ "authenticatorId": id }))
            .await?;
        // serde diagnostics can echo rejected values, including privateKey.
        let reply: Credentials = serde_json::from_value(result)
            .map_err(|_| Error::protocol("WebAuthn.getCredentials: invalid credential response"))?;
        reply
            .credentials
            .into_iter()
            .map(PasskeyCredential::try_from)
            .collect()
    }

    /// Remove one credential by its raw credential ID bytes.
    pub async fn remove_credential(
        &mut self,
        id: &VirtualAuthenticatorId,
        credential_id: &[u8],
    ) -> Result<()> {
        self.webauthn_call(
            "WebAuthn.removeCredential",
            json!({ "authenticatorId": id, "credentialId": STANDARD.encode(credential_id) }),
        )
        .await?;
        Ok(())
    }

    /// Delete all in-memory credentials from one virtual authenticator.
    pub async fn clear_credentials(&mut self, id: &VirtualAuthenticatorId) -> Result<()> {
        self.webauthn_call(
            "WebAuthn.clearCredentials",
            json!({ "authenticatorId": id }),
        )
        .await?;
        Ok(())
    }

    async fn webauthn_call(&mut self, method: &str, params: Value) -> Result<Value> {
        if self.cdp.session_id().is_none() {
            return Err(Error::Refused(
                "WebAuthn requires an attached page target".into(),
            ));
        }
        self.cdp.call(method, params).await
    }
}
