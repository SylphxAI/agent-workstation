//! Caller-resolved secrets.
//!
//! The session never stores or resolves secrets itself: a fill names a
//! `secret_ref` and the caller's [`SecretResolver`] turns it into a
//! [`Secret`] for the duration of one insertion. A `Secret` has no `Display`,
//! a redacted `Debug`, is not serializable, and is overwritten on drop.

use async_trait::async_trait;

use crate::error::Result;

/// A secret value held only while it is typed into a page.
pub struct Secret(String);

impl Secret {
    /// Wrap a resolved value.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value, for insertion only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Overwrite in place: clearing keeps the allocation and the same
        // number of one-byte NULs refills it before it is freed.
        let len = self.0.len();
        self.0.clear();
        self.0.extend(std::iter::repeat_n('\0', len));
    }
}

/// Resolves a caller-defined secret reference (for example an item path in
/// the caller's secret store) to its value.
#[async_trait]
pub trait SecretResolver: Send + Sync + std::fmt::Debug {
    /// Resolve `secret_ref`. Errors must not contain the value.
    async fn resolve(&self, secret_ref: &str) -> Result<Secret>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2".into());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(s.expose(), "hunter2");
    }
}
