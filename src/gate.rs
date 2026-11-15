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
    /// A trusted supervisor confirmed the observed handoff was complete.
    Confirmed,
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
    state: std::sync::Mutex<State>,
    idle_timeout: Duration,
    pointer: std::sync::Mutex<Option<(f64, f64)>>,
    hand_backs: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    lease: Option<Lease>,
    lease_id: Option<Arc<()>>,
    cycle: Option<Arc<()>>,
    last_hand_back: Option<HandBack>,
    closed: bool,
}

/// Allocation identities distinguish sessions, leases and takeover cycles.
#[derive(Clone, Debug)]
pub(crate) struct HandoffToken {
    lease: Arc<()>,
    cycle: Arc<()>,
}

impl ControlGate {
    /// A gate with the agent in control.
    pub fn new() -> Arc<Self> {
        Self::with_idle_timeout(IDLE_TIMEOUT)
    }

    /// A gate whose human control ends after the timeout without input.
    pub fn with_idle_timeout(idle_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            holder: watch::channel(Holder::Agent).0,
            state: std::sync::Mutex::new(State::default()),
            idle_timeout,
            pointer: std::sync::Mutex::new(None),
            hand_backs: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Acquire the lease or confirm the same principal already holds it.
    pub fn acquire(&self, principal: &str) -> Result<Lease> {
        if principal.trim().is_empty() {
            return Err(Error::InvalidInput("empty agent principal".into()));
        }
        let mut s = self.state.lock().expect("state lock");
        if s.closed {
            return Err(Error::Closed);
        }
        match &s.lease {
            Some(l) if l.principal == principal => Ok(l.clone()),
            Some(l) => Err(Error::Leased {
                principal: l.principal.clone(),
                since: l.since,
            }),
            None => {
                let l = Lease {
                    principal: principal.to_string(),
                    since: Utc::now(),
                };
                s.lease = Some(l.clone());
                s.lease_id = Some(Arc::new(()));
                Ok(l)
            }
        }
    }

    /// Release the owning lease and invalidate its handoff handles.
    pub fn release_lease(&self, principal: &str) -> bool {
        let mut s = self.state.lock().expect("state lock");
        if s.lease.as_ref().is_some_and(|l| l.principal == principal) {
            s.lease = None;
            s.lease_id = None;
            s.cycle = None;
            true
        } else {
            false
        }
    }

    /// The current lease, if any.
    pub fn lease(&self) -> Option<Lease> {
        self.state.lock().expect("state lock").lease.clone()
    }

    fn check_lease(s: &State, principal: &str) -> Result<()> {
        if s.closed {
            return Err(Error::Closed);
        }
        match &s.lease {
            Some(l) if l.principal == principal => Ok(()),
            Some(l) => Err(Error::Leased {
                principal: l.principal.clone(),
                since: l.since,
            }),
            None => Err(Error::Refused(format!(
                "{principal} does not hold this desktop's lease; acquire it first"
            ))),
        }
    }

    /// Succeeds when the principal holds the lease and no person holds control.
    pub fn check(&self, principal: &str) -> Result<()> {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        Self::check_lease(&s, principal)?;
        self.check_agent_locked(&s)
    }

    /// Record where the desktop pointer is now (in output pixels).
    pub fn note_pointer(&self, x: f64, y: f64) {
        *self.pointer.lock().expect("pointer lock") = Some((x, y));
    }
    /// Where the desktop pointer was last placed, or None before input.
    pub fn pointer(&self) -> Option<(f64, f64)> {
        *self.pointer.lock().expect("pointer lock")
    }

    /// Current holder, after expiring an idle person.
    pub fn holder(&self) -> Holder {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        self.holder.borrow().clone()
    }
    fn check_agent_locked(&self, s: &State) -> Result<()> {
        if s.closed {
            return Err(Error::Closed);
        }
        match &*self.holder.borrow() {
            Holder::Agent => Ok(()),
            Holder::Human { since, .. } => Err(Error::HumanInControl { since: *since }),
        }
    }
    /// Succeeds when the agent may act; HumanInControl otherwise.
    pub fn check_agent(&self) -> Result<()> {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        self.check_agent_locked(&s)
    }
    /// Whether a person's input may reach the display now.
    pub fn human_may_input(&self) -> bool {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        !s.closed && matches!(*self.holder.borrow(), Holder::Human { .. })
    }
    /// A person takes control. Repeated takes extend the same active cycle.
    pub fn take(&self, principal: Option<String>) -> Holder {
        let mut s = self.state.lock().expect("state lock");
        if s.closed {
            return self.holder.borrow().clone();
        }
        let now = Utc::now();
        self.expire_idle(&mut s, now);
        self.holder.send_if_modified(|h| match h {
            Holder::Agent => {
                s.cycle = s.lease_id.as_ref().map(|_| Arc::new(()));
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
        let mut s = self.state.lock().expect("state lock");
        if s.closed {
            return;
        }
        self.expire_idle(&mut s, Utc::now());
        self.holder.send_if_modified(|h| {
            if let Holder::Human { last_input, .. } = h {
                *last_input = Utc::now();
            }
            false
        });
    }
    /// The person hands back; false if control already returned or closed.
    pub fn release(&self, note: Option<String>) -> bool {
        let mut s = self.state.lock().expect("state lock");
        self.hand_back(&mut s, HandBack::Released { note })
    }
    /// The most recent hand-back, consumed once.
    pub fn take_hand_back(&self) -> Option<HandBack> {
        self.state.lock().expect("state lock").last_hand_back.take()
    }
    /// Watch holder changes. A watch retains only the latest holder; use
    /// hand_backs to detect intervening cycles. Drop watch borrows before
    /// calling gate methods (as required by Tokio watch's blocking writes).
    pub fn subscribe(&self) -> watch::Receiver<Holder> {
        self.holder.subscribe()
    }
    /// Hand-back count. Increases before subscribers are notified.
    pub fn hand_backs(&self) -> u64 {
        self.hand_backs.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Wait until agent control returns or the session closes.
    pub async fn wait_for_agent(&self) {
        let mut rx = self.subscribe();
        loop {
            let h = self.holder();
            if matches!(h, Holder::Agent) || self.state.lock().expect("state lock").closed {
                return;
            }
            let wait = match h {
                Holder::Human { last_input, .. } => {
                    let deadline = last_input
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
    // Called only while holding state; idle testing and transition are atomic.
    fn expire_idle(&self, s: &mut State, now: DateTime<Utc>) {
        let idle = chrono::Duration::from_std(self.idle_timeout).unwrap_or_default();
        let expired = matches!(&*self.holder.borrow(), Holder::Human { last_input, .. } if now - *last_input >= idle);
        if expired && !s.closed {
            self.hand_back(s, HandBack::Idle);
        }
    }
    fn hand_back(&self, s: &mut State, why: HandBack) -> bool {
        if s.closed {
            return false;
        }
        self.holder.send_if_modified(|h| {
            if matches!(h, Holder::Human { .. }) {
                s.cycle = None;
                s.last_hand_back = Some(why);
                *h = Holder::Agent;
                self.hand_backs
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                true
            } else {
                false
            }
        })
    }
    pub(crate) fn handoff(&self, principal: &str) -> Result<HandoffToken> {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        Self::check_lease(&s, principal)?;
        match (&s.lease_id, &s.cycle) {
            (Some(lease), Some(cycle)) => Ok(HandoffToken {
                lease: lease.clone(),
                cycle: cycle.clone(),
            }),
            _ => Err(Error::StaleHandoff),
        }
    }
    fn validate(s: &State, t: &HandoffToken) -> Result<()> {
        if s.closed {
            return Err(Error::Closed);
        }
        if s.lease_id
            .as_ref()
            .is_some_and(|id| Arc::ptr_eq(id, &t.lease))
            && s.cycle.as_ref().is_some_and(|id| Arc::ptr_eq(id, &t.cycle))
        {
            Ok(())
        } else {
            Err(Error::StaleHandoff)
        }
    }
    pub(crate) fn check_handoff(&self, t: &HandoffToken) -> Result<()> {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        Self::validate(&s, t)
    }
    pub(crate) fn complete_handoff(&self, t: &HandoffToken) -> Result<()> {
        let mut s = self.state.lock().expect("state lock");
        self.expire_idle(&mut s, Utc::now());
        Self::validate(&s, t)?;
        self.hand_back(&mut s, HandBack::Confirmed);
        Ok(())
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().expect("state lock").closed
    }
    pub(crate) fn close(&self) {
        let mut s = self.state.lock().expect("state lock");
        s.closed = true;
        s.lease = None;
        s.lease_id = None;
        s.cycle = None;
        self.holder.send_modify(|_| {});
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
    #[test]
    fn handoff_is_bound_to_cycle_lease_generation_and_session() {
        let g = ControlGate::new();
        assert!(matches!(g.handoff("a"), Err(Error::Refused(_))));
        g.acquire("a").unwrap();
        assert!(matches!(g.handoff("a"), Err(Error::StaleHandoff)));
        g.take(None);
        let old = g.handoff("a").unwrap();
        g.acquire("a").unwrap(); // same live lease is stable
        g.take(Some("human".into())); // activity is not a new cycle
        g.check_handoff(&old).unwrap();
        assert!(matches!(g.check("a"), Err(Error::HumanInControl { .. })));
        assert!(matches!(g.handoff("b"), Err(Error::Leased { .. })));
        let other = ControlGate::new();
        other.acquire("a").unwrap();
        other.take(None);
        assert!(matches!(
            other.check_handoff(&old),
            Err(Error::StaleHandoff)
        ));
        g.release(None);
        g.take(None);
        assert!(matches!(g.complete_handoff(&old), Err(Error::StaleHandoff)));
        assert!(matches!(g.holder(), Holder::Human { .. }));
        let current = g.handoff("a").unwrap();
        assert!(g.release_lease("a"));
        g.acquire("a").unwrap(); // same name, different lease generation
        assert!(matches!(
            g.check_handoff(&current),
            Err(Error::StaleHandoff)
        ));
        // Reacquiring a lease cannot reinterpret an already-running takeover.
        assert!(matches!(g.handoff("a"), Err(Error::StaleHandoff)));
        g.release(None);
        g.take(None);
        g.check_handoff(&g.handoff("a").unwrap()).unwrap();
    }

    #[test]
    fn only_one_concurrent_completion_and_reasons_are_distinct() {
        let g = ControlGate::new();
        g.acquire("a").unwrap();
        g.take(None);
        let t = g.handoff("a").unwrap();
        let winners = std::thread::scope(|s| {
            let jobs: Vec<_> = (0..16)
                .map(|_| s.spawn(|| g.complete_handoff(&t).is_ok()))
                .collect();
            jobs.into_iter()
                .map(|j| usize::from(j.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(winners, 1);
        assert_eq!(g.hand_backs(), 1);
        assert_eq!(g.take_hand_back(), Some(HandBack::Confirmed));
        assert!(g.check("a").is_ok());
        g.take(None);
        g.release(Some("done".into()));
        assert_eq!(
            g.take_hand_back(),
            Some(HandBack::Released {
                note: Some("done".into())
            })
        );
        assert_eq!(g.hand_backs(), 2);
    }

    #[test]
    fn idle_and_closure_revoke_receipts_without_confirming() {
        let g = ControlGate::with_idle_timeout(Duration::from_secs(60));
        g.acquire("a").unwrap();
        g.take(None);
        let t = g.handoff("a").unwrap();
        // Advance the exact input deadline deterministically, without sleeping.
        g.holder.send_modify(|h| {
            if let Holder::Human { last_input, .. } = h {
                *last_input = Utc::now() - chrono::Duration::seconds(61);
            }
        });
        assert!(matches!(g.complete_handoff(&t), Err(Error::StaleHandoff)));
        assert_eq!(g.take_hand_back(), Some(HandBack::Idle));
        g.take(None);
        let t = g.handoff("a").unwrap();
        g.close();
        assert!(matches!(g.check_handoff(&t), Err(Error::Closed)));
        assert!(matches!(g.complete_handoff(&t), Err(Error::Closed)));
        assert!(matches!(g.acquire("a"), Err(Error::Closed)));
        assert!(!g.release(None));
        assert!(!g.human_may_input());
        assert_eq!(g.take_hand_back(), None);
        assert_eq!(g.hand_backs(), 1);
    }
}
