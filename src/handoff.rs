//! Restricted, cycle-fenced evidence for a trusted host supervisor.
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::browser::Web;
use crate::gate::HandoffToken;
use crate::{ControlGate, Error, Result};

/// Live control metadata. Values (including default values) are never returned.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FormState {
    /// Lowercase input or textarea tag.
    pub tag: String,
    /// DOM id, bounded to 1024 UTF-16 code units.
    pub id: String,
    /// DOM name, bounded to 1024 UTF-16 code units.
    pub name: String,
    /// Whether the current DOM value property is nonempty.
    pub value_nonempty: bool,
}

/// Neutral CSS-selector evidence, in request order.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectorState {
    /// At least one matching element exists.
    pub matched: bool,
    /// A match passes Element.checkVisibility with opacity and visibility checks.
    /// This does not establish that it is unobscured or in the viewport.
    pub visible: bool,
}

/// Bounded evidence from the session-selected target's top-level light DOM.
/// Does not inspect subframes, shadow roots, text, HTML, or form values.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PageEvidence {
    /// Exact selected browser target (not necessarily the human's foreground tab).
    pub target_id: String,
    /// Current document URL, bounded to 8192 UTF-16 code units.
    pub url: String,
    /// At most 256 input/textarea controls in document order.
    pub forms: Vec<FormState>,
    /// Match/visibility booleans for at most 32 caller selectors.
    pub selectors: Vec<SelectorState>,
    /// Any URL/metadata/control/match limit was exceeded. Completion is refused.
    pub truncated: bool,
}

/// Trusted supervisor's handle to exactly one lease and human-control cycle.
/// The handle cannot activate a target, inject input, or run caller scripts.
#[derive(Debug)]
pub struct Handoff<'a> {
    gate: &'a ControlGate,
    web: &'a Mutex<Web>,
    token: HandoffToken,
}

impl<'a> Handoff<'a> {
    pub(crate) fn new(gate: &'a ControlGate, web: &'a Mutex<Web>, token: HandoffToken) -> Self {
        Self { gate, web, token }
    }

    /// Read fixed evidence without relaxing ordinary Agent gating. CSS selectors
    /// are data only: at most 32, each nonempty and at most 1024 UTF-8 bytes.
    /// Invalid selectors or unreadable/replaced documents return an error, not
    /// negative evidence. The cycle is rechecked after mutex/read awaits.
    pub async fn observe(&self, selectors: &[&str]) -> Result<HandoffObservation<'a>> {
        validate_selectors(selectors)?;
        self.gate.check_handoff(&self.token)?;
        let mut web = self.web.lock().await;
        self.gate.check_handoff(&self.token)?;
        let result = web.handoff_evidence(selectors).await;
        self.gate.check_handoff(&self.token)?;
        Ok(HandoffObservation {
            gate: self.gate,
            token: self.token.clone(),
            evidence: result?,
        })
    }
}

pub(crate) fn validate_selectors(selectors: &[&str]) -> Result<()> {
    if selectors.len() > 32 || selectors.iter().any(|s| s.is_empty() || s.len() > 1024) {
        return Err(Error::InvalidInput(
            "expected at most 32 nonempty CSS selectors, each at most 1024 bytes".into(),
        ));
    }
    Ok(())
}

/// An immutable read receipt. Only this receipt can confirm its exact cycle.
/// The library makes no claim that a challenge or any caller task is solved.
#[derive(Debug)]
pub struct HandoffObservation<'a> {
    gate: &'a ControlGate,
    token: HandoffToken,
    evidence: PageEvidence,
}

impl HandoffObservation<'_> {
    /// Inspect evidence before deciding whether the caller's predicate passed.
    pub fn evidence(&self) -> &PageEvidence {
        &self.evidence
    }

    /// After caller verification, consume this receipt and hand control back.
    /// No page read or automatic success inference occurs here. Truncated
    /// evidence is refused; a stale lease/cycle is StaleHandoff, closure Closed.
    /// Concurrent receipts can complete at most once; later cycles are untouched.
    pub fn complete(self) -> Result<()> {
        self.gate.check_handoff(&self.token)?;
        if self.evidence.truncated {
            return Err(Error::Refused("truncated handoff evidence".into()));
        }
        self.gate.complete_handoff(&self.token)
    }
}
