//! Human takeover: who controls the session's display right now.
//!
//! [`ControlGate`] is the single authority both views and the takeover server
//! consult. While a person holds control, [`ControlGate::check_agent`] returns
//! [`Error::HumanInControl`] for every agent action and observation, and only
//! the person's VNC input reaches the display. Handing back (explicitly, or
//! after [`IDLE_TIMEOUT`] without input) resumes the agent.

pub mod lifecycle;
pub mod server;
pub mod vnc;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use crate::error::{Error, Result};

/// A person's control ends after this long without input.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Who drives the display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The agent drives through the browser and computer views.
    Agent,
    /// A person drives through the takeover client.
    Human {
        /// When they took control.
        since: DateTime<Utc>,
        /// Caller-supplied label for audit (never interpreted).
        principal: Option<String>,
        /// Their last input.
        last_input: DateTime<Utc>,
    },
}

/// Why control returned to the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandBack {
    /// The person handed back, optionally with a note for the agent.
    Released {
        /// Free text the person left for the agent.
        note: Option<String>,
    },
    /// No input for [`IDLE_TIMEOUT`].
    Idle,
}

/// Shared control state for one session.
#[derive(Debug)]
pub struct ControlGate {
    holder: watch::Sender<Holder>,
    last_hand_back: std::sync::Mutex<Option<HandBack>>,
    idle_timeout: Duration,
}

impl ControlGate {
    /// A gate with the agent in control.
    pub fn new() -> Arc<Self> {
        Self::with_idle_timeout(IDLE_TIMEOUT)
    }

    /// A gate whose human control ends after `idle_timeout` without input.
    pub fn with_idle_timeout(idle_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            holder: watch::channel(Holder::Agent).0,
            last_hand_back: std::sync::Mutex::new(None),
            idle_timeout,
        })
    }

    /// Current holder, after expiring an idle person.
    pub fn holder(&self) -> Holder {
        self.expire_idle(Utc::now());
        self.holder.borrow().clone()
    }

    /// `Ok` when the agent may act; [`Error::HumanInControl`] otherwise.
    pub fn check_agent(&self) -> Result<()> {
        match self.holder() {
            Holder::Agent => Ok(()),
            Holder::Human { since, .. } => Err(Error::HumanInControl { since }),
        }
    }

    /// Whether a person's input may reach the display now.
    pub fn human_may_input(&self) -> bool {
        matches!(self.holder(), Holder::Human { .. })
    }

    /// A person takes control. Idempotent for an already-controlling person.
    pub fn take(&self, principal: Option<String>) -> Holder {
        let now = Utc::now();
        self.holder.send_if_modified(|h| match h {
            Holder::Agent => {
                *h = Holder::Human {
                    since: now,
                    principal,
                    last_input: now,
                };
                true
            }
            Holder::Human { last_input, .. } => {
                *last_input = now;
                false
            }
        });
        self.holder.borrow().clone()
    }

    /// Record a person's input, extending their control.
    pub fn note_input(&self) {
        let now = Utc::now();
        self.holder.send_if_modified(|h| {
            if let Holder::Human { last_input, .. } = h {
                *last_input = now;
            }
            false
        });
    }

    /// The person hands back. Returns `false` when the agent already held control.
    pub fn release(&self, note: Option<String>) -> bool {
        self.hand_back(HandBack::Released { note })
    }

    /// The most recent hand-back, consumed once so the agent can re-observe
    /// and read the person's note.
    pub fn take_hand_back(&self) -> Option<HandBack> {
        self.last_hand_back.lock().expect("hand-back lock").take()
    }

    /// Watch holder changes (for UIs and long-polls).
    pub fn subscribe(&self) -> watch::Receiver<Holder> {
        self.holder.subscribe()
    }

    /// Wait until the agent holds control again.
    pub async fn wait_for_agent(&self) {
        let mut rx = self.subscribe();
        loop {
            if matches!(self.holder(), Holder::Agent) {
                return;
            }
            let wait = match &*rx.borrow() {
                Holder::Human { last_input, .. } => {
                    let deadline = *last_input
                        + chrono::Duration::from_std(self.idle_timeout).unwrap_or_default();
                    (deadline - Utc::now())
                        .to_std()
                        .unwrap_or(Duration::ZERO)
                        .max(Duration::from_millis(50))
                }
                Holder::Agent => Duration::ZERO,
            };
            let _ = tokio::time::timeout(wait, rx.changed()).await;
        }
    }

    fn expire_idle(&self, now: DateTime<Utc>) {
        let idle = chrono::Duration::from_std(self.idle_timeout).unwrap_or_default();
        let expired = matches!(&*self.holder.borrow(), Holder::Human { last_input, .. } if now - *last_input >= idle);
        if expired {
            self.hand_back(HandBack::Idle);
        }
    }

    fn hand_back(&self, why: HandBack) -> bool {
        let changed = self.holder.send_if_modified(|h| {
            if matches!(h, Holder::Human { .. }) {
                *h = Holder::Agent;
                true
            } else {
                false
            }
        });
        if changed {
            *self.last_hand_back.lock().expect("hand-back lock") = Some(why);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_is_refused_while_a_person_holds_control_and_resumes_on_release() {
        let gate = ControlGate::new();
        assert!(gate.check_agent().is_ok());
        assert!(!gate.human_may_input());
        gate.take(Some("viewer".into()));
        let err = gate.check_agent().unwrap_err();
        assert_eq!(err.code(), "human-in-control");
        assert!(gate.human_may_input());
        assert!(gate.release(Some("logged in".into())));
        assert!(gate.check_agent().is_ok());
        assert_eq!(
            gate.take_hand_back(),
            Some(HandBack::Released {
                note: Some("logged in".into())
            })
        );
        assert_eq!(gate.take_hand_back(), None);
        assert!(!gate.release(None), "second release is a no-op");
    }

    #[tokio::test]
    async fn idle_person_hands_back_automatically() {
        let gate = ControlGate::with_idle_timeout(Duration::from_millis(60));
        gate.take(None);
        assert!(gate.check_agent().is_err());
        tokio::time::timeout(Duration::from_secs(2), gate.wait_for_agent())
            .await
            .expect("idle hand-back");
        assert!(gate.check_agent().is_ok());
        assert_eq!(gate.take_hand_back(), Some(HandBack::Idle));
    }
}
