//! Who controls the session desktop right now: which agent, or a person.
//!
//! [`ControlGate`] is the single authority both views and the takeover server
//! consult. It answers two questions:
//!
//! - **Which agent owns the session.** One agent principal at a time holds
//!   the session's [`Lease`]; every other principal gets [`Error::Leased`]
//!   until the holder releases it or the session closes.
//! - **Whether a person drives it.** While a person holds control,
//!   [`ControlGate::check_agent`] returns [`Error::HumanInControl`] for every
//!   agent action and observation, and only the person's VNC input reaches
//!   the display. Handing back (explicitly, or after [`IDLE_TIMEOUT`] without
//!   input) resumes the lease holder.
//!
//! It also records where the desktop pointer is, whichever side moved it
//! last.

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

/// One agent principal's exclusive hold on a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// Caller-defined agent identity (compared exactly, never interpreted).
    pub principal: String,
    /// When it was acquired.
    pub since: DateTime<Utc>,
}

/// Shared control state for one session.
#[derive(Debug)]
pub struct ControlGate {
    holder: watch::Sender<Holder>,
    last_hand_back: std::sync::Mutex<Option<HandBack>>,
    idle_timeout: Duration,
    lease: std::sync::Mutex<Option<Lease>>,
    pointer: std::sync::Mutex<Option<(f64, f64)>>,
    hand_backs: std::sync::atomic::AtomicU64,
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
            lease: std::sync::Mutex::new(None),
            pointer: std::sync::Mutex::new(None),
            hand_backs: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Acquire the lease for `principal`, or confirm that it already holds
    /// it. [`Error::Leased`] names the holder when another principal has it.
    pub fn acquire(&self, principal: &str) -> Result<Lease> {
        if principal.trim().is_empty() {
            return Err(Error::InvalidInput("empty agent principal".into()));
        }
        let mut lease = self.lease.lock().expect("lease lock");
        match &*lease {
            Some(held) if held.principal == principal => Ok(held.clone()),
            Some(held) => Err(Error::Leased {
                principal: held.principal.clone(),
                since: held.since,
            }),
            None => {
                let new = Lease {
                    principal: principal.to_string(),
                    since: Utc::now(),
                };
                *lease = Some(new.clone());
                Ok(new)
            }
        }
    }

    /// Release `principal`'s lease. `false` when it did not hold it.
    pub fn release_lease(&self, principal: &str) -> bool {
        let mut lease = self.lease.lock().expect("lease lock");
        if lease.as_ref().is_some_and(|l| l.principal == principal) {
            *lease = None;
            true
        } else {
            false
        }
    }

    /// The current lease, if any.
    pub fn lease(&self) -> Option<Lease> {
        self.lease.lock().expect("lease lock").clone()
    }

    /// `Ok` when `principal` holds the lease and no person holds control.
    pub fn check(&self, principal: &str) -> Result<()> {
        match &*self.lease.lock().expect("lease lock") {
            Some(l) if l.principal == principal => {}
            Some(l) => {
                return Err(Error::Leased {
                    principal: l.principal.clone(),
                    since: l.since,
                });
            }
            None => {
                return Err(Error::Refused(format!(
                    "{principal} does not hold this desktop's lease; acquire it first"
                )));
            }
        }
        self.check_agent()
    }

    /// Record where the desktop pointer is now (in output pixels).
    pub fn note_pointer(&self, x: f64, y: f64) {
        *self.pointer.lock().expect("pointer lock") = Some((x, y));
    }

    /// Where the desktop pointer is, as last placed by the agent or the
    /// person; `None` before anything placed it.
    pub fn pointer(&self) -> Option<(f64, f64)> {
        *self.pointer.lock().expect("pointer lock")
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

    /// Watch holder changes (for UIs and long-polls). A watch keeps only
    /// the latest holder, so a person who takes and hands back between two
    /// observations is seen through [`ControlGate::hand_backs`], not as a
    /// transition.
    pub fn subscribe(&self) -> watch::Receiver<Holder> {
        self.holder.subscribe()
    }

    /// How many times control has returned to the agent from a person. It
    /// increases before subscribers are notified.
    pub fn hand_backs(&self) -> u64 {
        self.hand_backs.load(std::sync::atomic::Ordering::Acquire)
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
        let mut last = self.last_hand_back.lock().expect("hand-back lock");
        let changed = self.holder.send_if_modified(|h| {
            if matches!(h, Holder::Human { .. }) {
                *h = Holder::Agent;
                self.hand_backs
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                true
            } else {
                false
            }
        });
        if changed {
            *last = Some(why);
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
        assert_eq!(gate.hand_backs(), 1);
        // An observer that wakes after a take and a hand-back sees only the
        // agent holding control, but the count moved.
        let mut rx = gate.subscribe();
        rx.borrow_and_update();
        gate.take(None);
        gate.release(None);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Holder::Agent);
        assert_eq!(gate.hand_backs(), 2);
    }

    #[test]
    fn one_principal_holds_the_lease_until_it_releases() {
        let gate = ControlGate::new();
        assert!(gate.check("a").is_err(), "no lease yet");
        let lease = gate.acquire("a").unwrap();
        assert_eq!(gate.acquire("a").unwrap(), lease, "re-acquiring is a no-op");
        assert!(gate.check("a").is_ok());
        let err = gate.acquire("b").unwrap_err();
        assert!(
            matches!(&err, Error::Leased { principal, .. } if principal == "a"),
            "{err}"
        );
        assert_eq!(gate.check("b").unwrap_err().code(), "leased");
        assert!(!gate.release_lease("b"), "only the holder releases");
        assert_eq!(gate.lease().map(|l| l.principal), Some("a".into()));
        // A person's control applies to the holder; others still see the lease.
        gate.take(None);
        assert_eq!(gate.check("a").unwrap_err().code(), "human-in-control");
        assert_eq!(gate.check("b").unwrap_err().code(), "leased");
        gate.release(None);
        assert!(gate.release_lease("a"));
        assert!(gate.check("a").is_err(), "a released view is refused");
        assert!(gate.acquire("b").is_ok());
        assert_eq!(gate.check("a").unwrap_err().code(), "leased");
        assert!(gate.acquire(" ").is_err());
    }

    #[test]
    fn pointer_is_whatever_moved_it_last() {
        let gate = ControlGate::new();
        assert_eq!(gate.pointer(), None);
        gate.note_pointer(10.0, 20.0);
        gate.note_pointer(30.0, 40.0);
        assert_eq!(gate.pointer(), Some((30.0, 40.0)));
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
