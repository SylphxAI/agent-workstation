//! Portable desktop frames, input descriptions and window snapshots.

#[cfg(any(target_os = "linux", test))]
use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "linux", test))]
use crate::error::{Error, Result};

/// A captured desktop frame, RGBA8, top row first.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    /// Encode as PNG.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc
            .write_header()
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.write_image_data(&self.rgba)
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.finish()
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
        Ok(out)
    }
}

/// Pointer buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    /// Primary button.
    Left,
    /// Middle button.
    Middle,
    /// Secondary button.
    Right,
    /// Side button that browsers map to history back.
    Back,
    /// Extra button that browsers map to history forward.
    Forward,
}

/// One pointer or keyboard step.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Move to output pixel coordinates.
    Move { x: f64, y: f64 },
    /// Press or release a button.
    Button { button: Button, pressed: bool },
    /// Scroll by notches (positive = down/right).
    Scroll { dx: i32, dy: i32 },
    /// Press a chord such as `ctrl+l` or `Return`, then release it.
    Key(String),
    /// Press the keys of a chord (`shift`, `ctrl+alt`, `w`) and keep them
    /// down: held modifiers apply to later pointer and key steps. Keys still
    /// held when the steps end are released then.
    KeyDown(String),
    /// Release keys held by [`Input::KeyDown`].
    KeyUp(String),
    /// Type text.
    Text(String),
    /// Pause between steps.
    Wait(Duration),
}

/// A desktop bounding rectangle in output pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopRect {
    /// Left coordinate in pixels.
    pub x: i32,
    /// Top coordinate in pixels.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl DesktopRect {
    /// Centre point of the rectangle in output pixels.
    pub fn center(&self) -> (f64, f64) {
        (
            f64::from(self.x) + f64::from(self.width) / 2.0,
            f64::from(self.y) + f64::from(self.height) / 2.0,
        )
    }
}

/// A mapped window on the session desktop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopWindow {
    /// Compositor-assigned numeric window identifier.
    pub id: u64,
    /// Window title.
    pub name: String,
    /// Application identifier (Wayland `app_id` or window class), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Owning process id, when reported by the compositor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Whether this window currently holds keyboard focus.
    pub focused: bool,
    /// Bounding box in desktop pixels.
    pub rect: DesktopRect,
}

/// A window paired with its snapshot ref (`e1`, `e2`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopWindowRef {
    /// Ref identifier (snapshot-qualified in daemon sessions).
    #[serde(rename = "ref")]
    pub ref_id: String,
    /// Underlying window metadata.
    #[serde(flatten)]
    pub window: DesktopWindow,
}

/// A structured snapshot of the desktop window tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSnapshot {
    /// Snapshot id, unique within the session (`d1`, `d2`, ...).
    /// Qualified refs (`@<id>:e1`) name this snapshot explicitly.
    pub id: String,
    /// Compact outline with window refs.
    pub text: String,
    /// Mapped windows in deterministic ref order.
    pub windows: Vec<DesktopWindowRef>,
    /// Number of refs issued.
    pub refs: usize,
}

/// The source of a session desktop snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionDesktopSnapshot {
    /// Nodes and controls from the session's private native accessibility bus.
    NativeAccessibility {
        /// Bounded tree with fully qualified refs.
        #[serde(flatten)]
        tree: crate::platform::DesktopTree,
    },
    /// Only compositor windows are available; refs cannot name controls.
    Compositor {
        /// Why the native accessibility tree was not used.
        fallback_reason: DesktopFallbackReason,
        /// Window-only snapshot.
        #[serde(flatten)]
        snapshot: DesktopSnapshot,
        /// Whether the window list has more results after this page.
        truncated: bool,
    },
}

/// Result of focusing a desktop target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DesktopFocus {
    /// An accessible control accepted native focus.
    NativeAccessibility {
        /// Ref that received focus.
        target: crate::platform::NodeRef,
    },
    /// A compositor window received focus.
    Compositor {
        /// Focused window.
        window: DesktopWindow,
    },
}

/// Machine-readable reason that a session desktop snapshot is window-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopFallbackReason {
    /// No private session D-Bus or AT-SPI service was reachable.
    AccessibilityUnavailable,
    /// The accessibility registry has not published any accessible applications.
    NoAccessibleApplications,
    /// The native accessibility read exceeded its deadline.
    AccessibilityTimeout,
}

/// Parsed desktop ref (`@e1`, `e1`, or a qualified ref).
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedDesktopRef {
    pub snapshot: Option<String>,
    pub id: String,
}

#[cfg(any(target_os = "linux", test))]
impl ParsedDesktopRef {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(Error::InvalidInput("empty desktop ref".into()));
        }
        let body = s.strip_prefix('@');
        let candidate = body.unwrap_or(s);
        let (snapshot, id) = match candidate.rsplit_once(':') {
            Some((snap, id)) if body.is_some() => (Some(snap), id),
            _ => (None, candidate),
        };
        let valid = id.len() > 1
            && id.starts_with('e')
            && id[1..].bytes().all(|b| b.is_ascii_digit())
            && snapshot.is_none_or(|snap| {
                !snap.is_empty()
                    && snap
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            });
        if valid {
            Ok(Self {
                snapshot: snapshot.map(str::to_owned),
                id: id.to_owned(),
            })
        } else {
            Err(Error::InvalidInput(format!(
                "{s:?} is not a desktop ref; refs look like @e1 or @<snapshot id>:e1"
            )))
        }
    }
}

/// Tracks the latest fallback snapshot for a session and enforces `StaleRef`.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default)]
pub(crate) struct DesktopSnapshotStore {
    next_seq: u64,
    scope: Option<String>,
    current_id: Option<String>,
    refs: HashMap<String, DesktopWindow>,
}

#[cfg(any(target_os = "linux", test))]
impl DesktopSnapshotStore {
    #[cfg(target_os = "linux")]
    pub(crate) fn scoped(scope: String) -> Self {
        Self {
            scope: Some(scope),
            ..Self::default()
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn clear(&mut self) {
        self.current_id = None;
        self.refs.clear();
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn is_active(&self) -> bool {
        self.current_id.is_some()
    }

    pub(crate) fn record(&mut self, windows: Vec<DesktopWindow>) -> DesktopSnapshot {
        self.next_seq += 1;
        let id = match &self.scope {
            Some(scope) => format!("{scope}.{}", self.next_seq),
            None => format!("d{}", self.next_seq),
        };
        let mut lines = Vec::with_capacity(windows.len());
        let mut entries = Vec::with_capacity(windows.len());
        self.refs.clear();
        for (idx, window) in windows.into_iter().enumerate() {
            let ref_id = format!("e{}", idx + 1);
            let (display_ref, public_ref) = if self.scope.is_some() {
                let qualified = format!("@{id}:{ref_id}");
                (qualified.clone(), qualified)
            } else {
                (format!("@{ref_id}"), ref_id.clone())
            };
            let app = window
                .app_id
                .as_deref()
                .map(|a| format!(" app={a}"))
                .unwrap_or_default();
            let focus = if window.focused { " focused" } else { "" };
            let title = window.name.replace('\n', " ");
            lines.push(format!(
                "window {:?}{app} id={} rect={},{} {}x{}{focus} [{display_ref}]",
                title,
                window.id,
                window.rect.x,
                window.rect.y,
                window.rect.width,
                window.rect.height,
            ));
            self.refs.insert(ref_id.clone(), window.clone());
            entries.push(DesktopWindowRef {
                ref_id: public_ref,
                window,
            });
        }
        self.current_id = Some(id.clone());
        let refs = entries.len();
        DesktopSnapshot {
            id,
            text: lines.join("\n"),
            windows: entries,
            refs,
        }
    }

    pub(crate) fn resolve(&self, raw: &str) -> Result<DesktopWindow> {
        let parsed = ParsedDesktopRef::parse(raw)?;
        if self.scope.is_some() && parsed.snapshot.is_none() {
            return Err(Error::StaleRef(
                "a qualified desktop ref is required".into(),
            ));
        }
        let Some(current) = &self.current_id else {
            return Err(Error::StaleRef(format!(
                "{raw} (no desktop snapshot taken yet)"
            )));
        };
        if let Some(snap) = &parsed.snapshot
            && snap != current
        {
            return Err(Error::StaleRef(format!(
                "{raw} belongs to snapshot {snap}, current is {current}"
            )));
        }
        self.refs
            .get(&parsed.id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("desktop ref {raw} in snapshot {current}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_window(id: u64, name: &str, focused: bool) -> DesktopWindow {
        DesktopWindow {
            id,
            name: name.into(),
            app_id: Some("chromium".into()),
            pid: Some(1234),
            focused,
            rect: DesktopRect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
        }
    }

    #[test]
    fn desktop_snapshot_assigns_refs_and_rejects_stale_snapshots() {
        let mut store = DesktopSnapshotStore::default();
        assert_eq!(store.resolve("@e1").unwrap_err().code(), "stale-ref");

        let s1 = store.record(vec![sample_window(7, "Main", true)]);
        assert_eq!(s1.id, "d1");
        assert_eq!(s1.refs, 1);
        assert!(s1.text.contains("window \"Main\""));
        assert!(s1.text.contains("[@e1]"));
        assert_eq!(store.resolve("@e1").unwrap().id, 7);
        assert_eq!(store.resolve("e1").unwrap().id, 7);
        assert_eq!(store.resolve("@d1:e1").unwrap().id, 7);
        assert_eq!(store.resolve("@e9").unwrap_err().code(), "not-found");
        assert_eq!(store.resolve("@bad").unwrap_err().code(), "invalid-input");

        let s2 = store.record(vec![sample_window(8, "Dialog", true)]);
        assert_eq!(s2.id, "d2");
        assert_eq!(store.resolve("@d1:e1").unwrap_err().code(), "stale-ref");
        assert_eq!(store.resolve("@d2:e1").unwrap().id, 8);
        assert_eq!(store.resolve("@e1").unwrap().id, 8);
    }

    #[test]
    fn desktop_rect_center_handles_negative_origins() {
        let r = DesktopRect {
            x: -100,
            y: 50,
            width: 200,
            height: 100,
        };
        assert_eq!(r.center(), (0.0, 100.0));
    }
}
