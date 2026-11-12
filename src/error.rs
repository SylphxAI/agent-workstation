//! The one error type every view, engine and server returns.

use chrono::{DateTime, Utc};

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything a workstation call can fail with.
///
/// [`Error::code`] is a stable, machine-readable slug; consumers map it onto
/// their own wire format. Messages never carry secret values, cookie contents
/// or typed credential text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A person holds control of the session. Agent actions and observations
    /// are refused until they hand back; retrying after release succeeds.
    #[error(
        "a person is in control of this session since {since}; agent actions resume after they hand back"
    )]
    HumanInControl {
        /// When the person took control.
        since: DateTime<Utc>,
    },
    /// The browser protocol (CDP) failed or answered with an error.
    #[error("browser protocol: {0}")]
    Protocol(String),
    /// The browser or display is still starting; retry after the hint.
    #[error("starting: {detail}")]
    Starting {
        /// What is starting.
        detail: String,
        /// Suggested wait before retrying.
        retry_after: Option<std::time::Duration>,
    },
    /// A wait exceeded its deadline.
    #[error("timed out: {0}")]
    Timeout(String),
    /// A named element, tab, ref or file does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller passed an argument this crate refuses.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The configured engine or backend does not offer this capability.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A helper process (display server, window manager, browser, VNC
    /// server) failed to start or exited.
    #[error("process: {0}")]
    Process(String),
    /// A profile directory or `fingerprint.json` could not be read or written.
    #[error("profile: {0}")]
    Profile(String),
    /// A request guard or credential-fill precondition refused the action.
    #[error("refused: {0}")]
    Refused(String),
    /// The session was closed.
    #[error("session closed")]
    Closed,
    /// Local I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Error::HumanInControl { .. } => "human-in-control",
            Error::Protocol(_) => "protocol",
            Error::Starting { .. } => "starting",
            Error::Timeout(_) => "timeout",
            Error::NotFound(_) => "not-found",
            Error::InvalidInput(_) => "invalid-input",
            Error::Unsupported(_) => "unsupported",
            Error::Process(_) => "process",
            Error::Profile(_) => "profile",
            Error::Refused(_) => "refused",
            Error::Closed => "closed",
            Error::Io(_) => "io",
        }
    }

    /// Whether the same call may succeed later without any change by the
    /// caller (a person handing back, a browser finishing start-up).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::HumanInControl { .. } | Error::Starting { .. } | Error::Timeout(_)
        )
    }

    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_in_control_is_transient_and_has_a_stable_code() {
        let e = Error::HumanInControl { since: Utc::now() };
        assert_eq!(e.code(), "human-in-control");
        assert!(e.is_transient());
        assert!(!Error::InvalidInput("x".into()).is_transient());
        assert_eq!(Error::protocol("x").code(), "protocol");
    }
}
