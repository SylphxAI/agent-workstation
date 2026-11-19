//! Fail-closed [`PlatformAdapter`] for operating systems without a native backend.

use async_trait::async_trait;

use crate::desktop::DesktopWindow;
use crate::platform::{
    AdapterError, DesktopAction, DesktopTree, PlatformAdapter, PlatformCapabilities, TreeOptions,
};

/// Fallback adapter that fails closed with [`AdapterError::UnsupportedPlatform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedPlatformAdapter;

impl UnsupportedPlatformAdapter {
    /// Construct the fail-closed unsupported-platform adapter.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlatformAdapter for UnsupportedPlatformAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            platform: std::env::consts::OS.into(),
            backend: "unsupported".into(),
            accessibility_tree: false,
            native_actions: false,
            keyboard_injection: false,
            pointer_injection: false,
            window_enumeration: false,
        }
    }

    async fn snapshot(&self, _options: TreeOptions) -> Result<DesktopTree, AdapterError> {
        Err(AdapterError::UnsupportedPlatform(format!(
            "native desktop accessibility adapter is not implemented for {}",
            std::env::consts::OS
        )))
    }

    async fn act(&self, _action: DesktopAction) -> Result<(), AdapterError> {
        Err(AdapterError::UnsupportedPlatform(format!(
            "native desktop actions are not implemented for {}",
            std::env::consts::OS
        )))
    }

    async fn list_windows(&self) -> Result<Vec<DesktopWindow>, AdapterError> {
        Err(AdapterError::UnsupportedPlatform(format!(
            "native window enumeration is not implemented for {}",
            std::env::consts::OS
        )))
    }
}
