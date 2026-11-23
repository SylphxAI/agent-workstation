//! Compile-time native host desktop accessibility & automation adapters.
//!
//! [`PlatformAdapter`] provides a uniform, pure-Rust interface over each host
//! operating system's native accessibility tree and input primitives — zero
//! screenshot pixel-matching, zero embedded models, and zero external host
//! scripts:
//!
//! - **Linux (`#[cfg(target_os = "linux")]`)**: AT-SPI2 over D-Bus plus
//!   Wayland/`sway` compositor integration.
//! - **macOS (`#[cfg(target_os = "macos")]`)**: Accessibility (`AXUIElement`),
//!   CoreFoundation, and CoreGraphics FFI.
//! - **Windows (`#[cfg(target_os = "windows")]`)**: UI Automation
//!   (`IUIAutomation` COM) and Win32 input.
//!
//! Every backend builds snapshots through [`SnapshotIndex`], guaranteeing
//! deterministic `@<snap>:eN` / `@eN` ref allocation, depth/node truncation
//! bounds, and `StaleRef` invalidation across snapshot rotations.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::desktop::{Button, DesktopRect, DesktopWindow};
use crate::error::Error;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod windows;

/// Hard ceiling on accessibility tree traversal depth.
pub const MAX_ALLOWED_DEPTH: usize = 64;
/// Hard ceiling on nodes materialized in a single accessibility tree snapshot.
pub const MAX_ALLOWED_NODES: usize = 10_000;
/// Default traversal depth when [`TreeOptions::default`] is used.
pub const DEFAULT_MAX_DEPTH: usize = 16;
/// Default node cap when [`TreeOptions::default`] is used.
pub const DEFAULT_MAX_NODES: usize = 500;

/// Errors returned by [`PlatformAdapter`] implementations.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// The supplied ref names a snapshot superseded by a newer snapshot (or no snapshot exists).
    #[error("stale ref: {0}")]
    StaleRef(String),
    /// The host OS denied accessibility or input injection permission.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// This operating system or desktop environment is not supported.
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    /// The ref was syntactically valid for the active snapshot, but no such node exists.
    #[error("node not found: {0}")]
    NodeNotFound(String),
    /// The supplied ref string does not match `@eN`, `eN`, or `@<snap>:eN`.
    #[error("invalid ref: {0}")]
    InvalidRef(String),
    /// A caller-supplied argument is malformed or out of bounds.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The target node or backend does not support the requested action.
    #[error("unsupported action: {0}")]
    UnsupportedAction(String),
    /// An OS accessibility or compositor IPC call failed.
    #[error("backend error: {0}")]
    Backend(String),
}

impl AdapterError {
    /// Stable machine-readable error slug.
    pub fn code(&self) -> &'static str {
        match self {
            AdapterError::StaleRef(_) => "stale-ref",
            AdapterError::PermissionDenied(_) => "permission-denied",
            AdapterError::UnsupportedPlatform(_) => "unsupported-platform",
            AdapterError::NodeNotFound(_) => "node-not-found",
            AdapterError::InvalidRef(_) => "invalid-ref",
            AdapterError::InvalidInput(_) => "invalid-input",
            AdapterError::UnsupportedAction(_) => "unsupported-action",
            AdapterError::Backend(_) => "backend-error",
        }
    }
}

impl From<AdapterError> for Error {
    fn from(err: AdapterError) -> Self {
        match err {
            AdapterError::StaleRef(m) => Error::StaleRef(m),
            AdapterError::PermissionDenied(m) => Error::Refused(m),
            AdapterError::UnsupportedPlatform(m) | AdapterError::UnsupportedAction(m) => {
                Error::Unsupported(m)
            }
            AdapterError::NodeNotFound(m) => Error::NotFound(m),
            AdapterError::InvalidRef(m) | AdapterError::InvalidInput(m) => Error::InvalidInput(m),
            AdapterError::Backend(m) => Error::Protocol(m),
        }
    }
}

/// Reference to a node in a [`DesktopTree`] (`@e1`, `e1`, or `@p1:e1`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeRef {
    /// Snapshot id (`p1`, `p2`, ...), when snapshot-qualified.
    pub snapshot: Option<String>,
    /// Snapshot-local element identifier (`e1`, `e2`, ...).
    pub id: String,
}

impl NodeRef {
    /// Construct a snapshot-qualified ref (`@<snapshot>:<id>`).
    pub fn qualified(snapshot: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            snapshot: Some(snapshot.into()),
            id: id.into(),
        }
    }

    /// Construct an unqualified ref against the latest snapshot (`@<id>`).
    pub fn latest(id: impl Into<String>) -> Self {
        Self {
            snapshot: None,
            id: id.into(),
        }
    }

    /// Parse `@e1`, `e1`, or `@<snapshot>:e1`. Rejects malformed refs fail-closed.
    pub fn parse(raw: &str) -> std::result::Result<Self, AdapterError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(AdapterError::InvalidRef("empty node ref".into()));
        }
        let body = s.strip_prefix('@');
        let candidate = body.unwrap_or(s);
        let (snapshot, id) = match candidate.rsplit_once(':') {
            Some((snap, id)) if body.is_some() => (Some(snap), id),
            _ => (None, candidate),
        };
        let valid_id =
            id.len() > 1 && id.starts_with('e') && id[1..].bytes().all(|b| b.is_ascii_digit());
        let valid_snap = snapshot.is_none_or(|snap| {
            !snap.is_empty()
                && snap
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        });
        if valid_id && valid_snap {
            Ok(Self {
                snapshot: snapshot.map(str::to_owned),
                id: id.to_owned(),
            })
        } else {
            Err(AdapterError::InvalidRef(format!(
                "{s:?} is not a valid node ref; expected @eN or @<snapshot>:eN"
            )))
        }
    }
}

impl fmt::Display for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.snapshot {
            Some(snap) => write!(f, "@{snap}:{}", self.id),
            None => write!(f, "@{}", self.id),
        }
    }
}

impl Serialize for NodeRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NodeRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        NodeRef::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A single node inside a [`DesktopTree`] accessibility snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopNode {
    /// Snapshot-local ref (`e1`, `e2`, ...).
    #[serde(rename = "ref")]
    pub ref_id: String,
    /// Snapshot-qualified ref (`@p1:e1`).
    pub qualified_ref: String,
    /// Normalized accessibility role (for example `window`, `button`, `text_field`).
    pub role: String,
    /// Accessible name or title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Current accessible value (for example input text or toggle state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Application identifier, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Owning process id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Whether the node is enabled for interaction.
    pub enabled: bool,
    /// Whether the node currently has keyboard focus.
    pub focused: bool,
    /// Screen-space bounding box in desktop pixels, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<DesktopRect>,
    /// Supported native accessibility action names (for example `press`, `focus`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    /// Child accessibility nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<DesktopNode>,
}

/// Structured accessibility tree returned by [`PlatformAdapter::snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopTree {
    /// Monotonic snapshot identifier (`p1`, `p2`, ...).
    pub snapshot_id: String,
    /// Host platform label (`linux`, `macos`, `windows`, `mock`, ...).
    pub platform: String,
    /// Whether nodes were omitted (limits, paging, or a partial backend read).
    pub truncated: bool,
    /// Total number of nodes included in this snapshot.
    pub total_nodes: usize,
    /// Effective depth cap applied during capture.
    pub max_depth: usize,
    /// Effective node cap applied during capture.
    pub max_nodes: usize,
    /// Compact indented accessibility outline with `[@<snap>:eN]` refs.
    pub text: String,
    /// Top-level root nodes (applications or windows).
    pub roots: Vec<DesktopNode>,
}

impl DesktopTree {
    /// Locate a node by `eN`, `@eN`, or `@<snap>:eN` within this tree.
    pub fn find_node(&self, raw: &str) -> Option<&DesktopNode> {
        let parsed = NodeRef::parse(raw).ok()?;
        if let Some(snap) = &parsed.snapshot
            && snap != &self.snapshot_id
        {
            return None;
        }
        fn walk<'a>(nodes: &'a [DesktopNode], id: &str) -> Option<&'a DesktopNode> {
            for n in nodes {
                if n.ref_id == id {
                    return Some(n);
                }
                if let Some(found) = walk(&n.children, id) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, &parsed.id)
    }

    /// Render a compact, indented accessibility outline for agent prompts.
    pub fn render_outline(&self) -> String {
        let mut lines = Vec::with_capacity(self.total_nodes + 1);
        fn write_nodes(nodes: &[DesktopNode], depth: usize, out: &mut Vec<String>) {
            let indent = "  ".repeat(depth);
            for n in nodes {
                let mut line = format!("{indent}{}", n.role);
                if !n.name.is_empty() {
                    let clean = n.name.replace('\n', " ");
                    line.push_str(&format!(" {clean:?}"));
                }
                if let Some(val) = &n.value
                    && !val.is_empty()
                {
                    let clean = val.replace('\n', " ");
                    line.push_str(&format!(" value={clean:?}"));
                }
                if !n.enabled {
                    line.push_str(" disabled");
                }
                if n.focused {
                    line.push_str(" focused");
                }
                if let Some(b) = n.bounds {
                    line.push_str(&format!(" ({},{} {}x{})", b.x, b.y, b.width, b.height));
                }
                line.push_str(&format!(" [{}]", n.qualified_ref));
                out.push(line);
                write_nodes(&n.children, depth + 1, out);
            }
        }
        write_nodes(&self.roots, 0, &mut lines);
        if self.truncated {
            lines.push(format!(
                "... [truncated; {} nodes, depth limit {}]",
                self.total_nodes, self.max_depth
            ));
        }
        lines.join("\n")
    }
}

/// Bounds and filtering options for [`PlatformAdapter::snapshot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeOptions {
    /// Maximum nesting depth to descend (clamped to `1..=MAX_ALLOWED_DEPTH`).
    pub max_depth: usize,
    /// Maximum total nodes to include (clamped to `1..=MAX_ALLOWED_NODES`).
    pub max_nodes: usize,
    /// Optional target process id filter (capture only that application when supported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
            pid: None,
        }
    }
}

impl TreeOptions {
    /// Effective depth and node caps after applying safety ceilings.
    pub fn effective_bounds(self) -> (usize, usize) {
        (
            self.max_depth.clamp(1, MAX_ALLOWED_DEPTH),
            self.max_nodes.clamp(1, MAX_ALLOWED_NODES),
        )
    }
}

/// Typed native desktop actions dispatched by [`PlatformAdapter::act`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DesktopAction {
    /// Invoke or click a node by ref.
    Click {
        /// Node ref (`@eN` or `@<snap>:eN`).
        #[serde(rename = "ref")]
        target: NodeRef,
        /// Optional pointer button (defaults to [`Button::Left`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<Button>,
    },
    /// Double-click a node by ref.
    DoubleClick {
        /// Node ref (`@eN` or `@<snap>:eN`).
        #[serde(rename = "ref")]
        target: NodeRef,
    },
    /// Move keyboard focus / raise a window or control by ref.
    Focus {
        /// Node ref (`@eN` or `@<snap>:eN`).
        #[serde(rename = "ref")]
        target: NodeRef,
    },
    /// Replace a control's accessible value directly without keystroke synthesis.
    SetValue {
        /// Node ref (`@eN` or `@<snap>:eN`).
        #[serde(rename = "ref")]
        target: NodeRef,
        /// Replacement text value.
        value: String,
    },
    /// Type Unicode text into the currently focused surface.
    TypeText {
        /// Text to type.
        text: String,
    },
    /// Press and release a key chord (for example `ctrl+l`, `cmd+v`, `Return`).
    PressKey {
        /// Key chord string.
        chord: String,
    },
    /// Scroll horizontally/vertically, optionally targeting a node ref's bounds.
    Scroll {
        /// Optional target node ref (`None` scrolls at current pointer position).
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        target: Option<NodeRef>,
        /// Horizontal scroll notches (positive = right).
        dx: i32,
        /// Vertical scroll notches (positive = down).
        dy: i32,
    },
}

/// Static and runtime capabilities reported by a [`PlatformAdapter`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Operating system or backend family (`linux`, `macos`, `windows`, `mock`, `unsupported`).
    pub platform: String,
    /// Concrete backend name (for example `linux-atspi2+wayland`, `macos-ax`, `windows-uia`).
    pub backend: String,
    /// Whether structured accessibility tree snapshots are supported.
    pub accessibility_tree: bool,
    /// Whether native accessibility actions (`Click`/`Focus`/`SetValue`) are supported.
    pub native_actions: bool,
    /// Whether keyboard text and key-chord injection is supported.
    pub keyboard_injection: bool,
    /// Whether pointer click and scroll injection is supported.
    pub pointer_injection: bool,
    /// Whether top-level window enumeration is supported.
    pub window_enumeration: bool,
}

/// Compile-time native host desktop automation contract.
///
/// Implementations maintain a monotonic snapshot index so refs from previous
/// snapshots are rejected with [`AdapterError::StaleRef`] after a newer
/// [`PlatformAdapter::snapshot`] rotates the tree.
#[async_trait]
pub trait PlatformAdapter: Send + Sync + fmt::Debug {
    /// Capabilities advertised by this adapter.
    fn capabilities(&self) -> PlatformCapabilities;

    /// Capture a bounded, structured accessibility tree with deterministic
    /// `@<snap>:eN` refs. Rotates the active snapshot generation.
    async fn snapshot(
        &self,
        options: TreeOptions,
    ) -> std::result::Result<DesktopTree, AdapterError>;

    /// Execute a typed desktop action against a node ref or focused surface.
    async fn act(&self, action: DesktopAction) -> std::result::Result<(), AdapterError>;

    /// Enumerate top-level desktop windows.
    async fn list_windows(&self) -> std::result::Result<Vec<DesktopWindow>, AdapterError>;
}

/// Construct the compile-time native [`PlatformAdapter`] for the current OS.
pub fn native_adapter() -> Box<dyn PlatformAdapter> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxPlatformAdapter::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOsPlatformAdapter::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsPlatformAdapter::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Box::new(unsupported::UnsupportedPlatformAdapter::new())
    }
}

/// Backend-internal raw tree node paired with an opaque native handle `H`.
#[derive(Clone, Debug)]
pub struct RawNode<H> {
    /// Backend-specific handle used to dispatch actions (for example D-Bus path, AX element, UIA runtime id).
    pub handle: H,
    /// Normalized role name.
    pub role: String,
    /// Accessible name or title.
    pub name: String,
    /// Current value, if any.
    pub value: Option<String>,
    /// Application id, if known.
    pub app_id: Option<String>,
    /// Process id, if known.
    pub pid: Option<u32>,
    /// Whether enabled.
    pub enabled: bool,
    /// Whether focused.
    pub focused: bool,
    /// Screen bounding box, if known.
    pub bounds: Option<DesktopRect>,
    /// Supported native action names.
    pub actions: Vec<String>,
    /// Child raw nodes.
    pub children: Vec<RawNode<H>>,
}

/// Snapshot generation registry shared by all [`PlatformAdapter`] backends.
///
/// Allocates deterministic pre-order `e1..eN` refs, enforces `max_depth` and
/// `max_nodes` truncation bounds, and rejects stale snapshot refs fail-closed.
#[derive(Debug)]
pub struct SnapshotIndex<H> {
    next_seq: u64,
    scope: Option<String>,
    current_id: Option<String>,
    entries: HashMap<String, (DesktopNode, H)>,
}

impl<H> Default for SnapshotIndex<H> {
    fn default() -> Self {
        Self {
            next_seq: 0,
            scope: None,
            current_id: None,
            entries: HashMap::new(),
        }
    }
}

impl<H> SnapshotIndex<H> {
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
        self.entries.clear();
    }

    /// Materialize a new [`DesktopTree`] from `raw_roots`, rotating the active
    /// snapshot id so qualified refs from earlier snapshots fail with
    /// [`AdapterError::StaleRef`].
    pub fn build(
        &mut self,
        platform: impl Into<String>,
        raw_roots: Vec<RawNode<H>>,
        options: TreeOptions,
    ) -> DesktopTree {
        let (max_depth, max_nodes) = options.effective_bounds();
        self.next_seq += 1;
        let snapshot_id = match &self.scope {
            Some(scope) => format!("{scope}.{}", self.next_seq),
            None => format!("p{}", self.next_seq),
        };
        self.entries.clear();
        struct BuildCtx<'a, H> {
            max_depth: usize,
            max_nodes: usize,
            snapshot_id: &'a str,
            counter: usize,
            truncated: bool,
            entries: &'a mut HashMap<String, (DesktopNode, H)>,
        }

        impl<H> BuildCtx<'_, H> {
            fn materialize(&mut self, raw: RawNode<H>, depth: usize) -> Option<DesktopNode> {
                if self.counter >= self.max_nodes {
                    self.truncated = true;
                    return None;
                }
                self.counter += 1;
                let ref_id = format!("e{}", self.counter);
                let qualified_ref = format!("@{}:{ref_id}", self.snapshot_id);
                let mut children = Vec::new();
                if depth < self.max_depth {
                    for child in raw.children {
                        if self.counter >= self.max_nodes {
                            self.truncated = true;
                            break;
                        }
                        if let Some(built) = self.materialize(child, depth + 1) {
                            children.push(built);
                        }
                    }
                } else if !raw.children.is_empty() {
                    self.truncated = true;
                }
                let node = DesktopNode {
                    ref_id: ref_id.clone(),
                    qualified_ref,
                    role: raw.role,
                    name: raw.name,
                    value: raw.value,
                    app_id: raw.app_id,
                    pid: raw.pid,
                    enabled: raw.enabled,
                    focused: raw.focused,
                    bounds: raw.bounds,
                    actions: raw.actions,
                    children,
                };
                let stored = DesktopNode {
                    children: Vec::new(),
                    ..node.clone()
                };
                self.entries.insert(ref_id, (stored, raw.handle));
                Some(node)
            }
        }

        let mut ctx = BuildCtx {
            max_depth,
            max_nodes,
            snapshot_id: &snapshot_id,
            counter: 0,
            truncated: false,
            entries: &mut self.entries,
        };
        let mut roots = Vec::new();
        for raw in raw_roots {
            if ctx.counter >= ctx.max_nodes {
                ctx.truncated = true;
                break;
            }
            if let Some(node) = ctx.materialize(raw, 1) {
                roots.push(node);
            }
        }
        let (total_nodes, truncated) = (ctx.counter, ctx.truncated);
        self.current_id = Some(snapshot_id.clone());
        let mut tree = DesktopTree {
            snapshot_id,
            platform: platform.into(),
            truncated,
            total_nodes,
            max_depth,
            max_nodes,
            text: String::new(),
            roots,
        };
        tree.text = tree.render_outline();
        tree
    }

    /// Resolve a [`NodeRef`] against the active snapshot generation.
    pub fn resolve(
        &self,
        target: &NodeRef,
    ) -> std::result::Result<(&DesktopNode, &H), AdapterError> {
        if self.scope.is_some() && target.snapshot.is_none() {
            return Err(AdapterError::StaleRef(
                "a qualified desktop ref is required".into(),
            ));
        }
        let Some(current) = &self.current_id else {
            return Err(AdapterError::StaleRef(format!(
                "{target} (no desktop accessibility snapshot taken yet)"
            )));
        };
        if let Some(snap) = &target.snapshot
            && snap != current
        {
            return Err(AdapterError::StaleRef(format!(
                "{target} belongs to snapshot {snap}, current is {current}"
            )));
        }
        self.entries
            .get(&target.id)
            .map(|(n, h)| (n, h))
            .ok_or_else(|| {
                AdapterError::NodeNotFound(format!("{target} not present in snapshot {current}"))
            })
    }
}

/// Parsed keyboard chord split into normalized modifier flags and primary key name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedChord {
    /// Control modifier (`ctrl`, `control`).
    pub ctrl: bool,
    /// Shift modifier (`shift`).
    pub shift: bool,
    /// Alt / Option modifier (`alt`, `opt`, `option`).
    pub alt: bool,
    /// Meta / Command / Super / Win modifier (`meta`, `cmd`, `command`, `super`, `win`).
    pub meta: bool,
    /// Primary non-modifier key token (for example `l`, `Return`, `Tab`, `Escape`).
    pub key: String,
}

/// Parse a chord such as `ctrl+shift+k`, `cmd+l`, or `Return`.
pub fn parse_chord(raw: &str) -> std::result::Result<ParsedChord, AdapterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::InvalidInput("empty key chord".into()));
    }
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut meta = false;
    let mut key: Option<String> = None;
    for part in trimmed.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return Err(AdapterError::InvalidInput(format!(
                "malformed key chord {raw:?}"
            )));
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "opt" | "option" => alt = true,
            "meta" | "cmd" | "command" | "super" | "win" => meta = true,
            _ => {
                if key.is_some() {
                    return Err(AdapterError::InvalidInput(format!(
                        "key chord {raw:?} has multiple primary keys"
                    )));
                }
                key = Some(token.to_owned());
            }
        }
    }
    let key = key.ok_or_else(|| {
        AdapterError::InvalidInput(format!("key chord {raw:?} is missing a primary key"))
    })?;
    Ok(ParsedChord {
        ctrl,
        shift,
        alt,
        meta,
        key,
    })
}

/// Recorded action entry captured by [`MockPlatformAdapter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedAction {
    /// Action dispatched by the caller.
    pub action: DesktopAction,
    /// Resolved native handle id when the action targeted a node ref.
    pub resolved_handle: Option<String>,
}

#[derive(Debug, Default)]
struct MockInner {
    roots: Vec<RawNode<String>>,
    index: SnapshotIndex<String>,
    permission_denied: Option<String>,
    actions: Vec<RecordedAction>,
}

/// Deterministic in-memory [`PlatformAdapter`] for unit and contract testing.
#[derive(Debug, Default)]
pub struct MockPlatformAdapter {
    inner: Mutex<MockInner>,
}

impl MockPlatformAdapter {
    /// Create a mock adapter seeded with `roots`.
    pub fn with_roots(roots: Vec<RawNode<String>>) -> Self {
        Self {
            inner: Mutex::new(MockInner {
                roots,
                index: SnapshotIndex::default(),
                permission_denied: None,
                actions: Vec::new(),
            }),
        }
    }

    /// Replace the live accessibility tree (for testing tree rotation & `StaleRef`).
    pub fn set_roots(&self, roots: Vec<RawNode<String>>) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).roots = roots;
    }

    /// Force fail-closed [`AdapterError::PermissionDenied`] on all operations when `Some`.
    pub fn set_permission_denied(&self, reason: Option<String>) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .permission_denied = reason;
    }

    /// Return all actions accepted by this mock adapter in order.
    pub fn recorded_actions(&self) -> Vec<RecordedAction> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .actions
            .clone()
    }
}

#[async_trait]
impl PlatformAdapter for MockPlatformAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            platform: "mock".into(),
            backend: "mock-in-memory".into(),
            accessibility_tree: true,
            native_actions: true,
            keyboard_injection: true,
            pointer_injection: true,
            window_enumeration: true,
        }
    }

    async fn snapshot(
        &self,
        options: TreeOptions,
    ) -> std::result::Result<DesktopTree, AdapterError> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = &inner.permission_denied {
            return Err(AdapterError::PermissionDenied(reason.clone()));
        }
        let filtered: Vec<RawNode<String>> = match options.pid {
            Some(pid) => inner
                .roots
                .iter()
                .filter(|r| r.pid == Some(pid))
                .cloned()
                .collect(),
            None => inner.roots.clone(),
        };
        Ok(inner.index.build("mock", filtered, options))
    }

    async fn act(&self, action: DesktopAction) -> std::result::Result<(), AdapterError> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = &inner.permission_denied {
            return Err(AdapterError::PermissionDenied(reason.clone()));
        }
        let resolved_handle = match &action {
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target }
            | DesktopAction::Focus { target }
            | DesktopAction::SetValue { target, .. } => {
                let (node, handle) = inner.index.resolve(target)?;
                if !node.enabled {
                    return Err(AdapterError::UnsupportedAction(format!(
                        "node {target} is disabled"
                    )));
                }
                Some(handle.clone())
            }
            DesktopAction::Scroll {
                target: Some(t), ..
            } => {
                let (_, handle) = inner.index.resolve(t)?;
                Some(handle.clone())
            }
            DesktopAction::Scroll { target: None, .. } | DesktopAction::TypeText { .. } => None,
            DesktopAction::PressKey { chord } => {
                parse_chord(chord)?;
                None
            }
        };
        if let DesktopAction::SetValue { value, .. } = &action
            && let Some(handle) = &resolved_handle
        {
            fn update_value(nodes: &mut [RawNode<String>], handle: &str, new_val: &str) {
                for n in nodes {
                    if n.handle == handle {
                        n.value = Some(new_val.to_owned());
                    }
                    update_value(&mut n.children, handle, new_val);
                }
            }
            update_value(&mut inner.roots, handle, value);
        }
        inner.actions.push(RecordedAction {
            action,
            resolved_handle,
        });
        Ok(())
    }

    async fn list_windows(&self) -> std::result::Result<Vec<DesktopWindow>, AdapterError> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = &inner.permission_denied {
            return Err(AdapterError::PermissionDenied(reason.clone()));
        }
        let mut out = Vec::new();
        for (i, root) in inner.roots.iter().enumerate() {
            if root.role == "window" {
                out.push(DesktopWindow {
                    id: (i + 1) as u64,
                    name: root.name.clone(),
                    app_id: root.app_id.clone(),
                    pid: root.pid,
                    focused: root.focused,
                    rect: root.bounds.unwrap_or_default(),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_raw_tree() -> Vec<RawNode<String>> {
        vec![RawNode {
            handle: "win-1".into(),
            role: "window".into(),
            name: "Settings".into(),
            value: None,
            app_id: Some("settings".into()),
            pid: Some(500),
            enabled: true,
            focused: true,
            bounds: Some(DesktopRect {
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
            }),
            actions: vec!["focus".into()],
            children: vec![
                RawNode {
                    handle: "input-email".into(),
                    role: "text_field".into(),
                    name: "Email".into(),
                    value: Some("old@example.com".into()),
                    app_id: Some("settings".into()),
                    pid: Some(500),
                    enabled: true,
                    focused: false,
                    bounds: Some(DesktopRect {
                        x: 40,
                        y: 80,
                        width: 300,
                        height: 36,
                    }),
                    actions: vec!["focus".into(), "set_value".into()],
                    children: vec![],
                },
                RawNode {
                    handle: "btn-save".into(),
                    role: "button".into(),
                    name: "Save".into(),
                    value: None,
                    app_id: Some("settings".into()),
                    pid: Some(500),
                    enabled: true,
                    focused: false,
                    bounds: Some(DesktopRect {
                        x: 40,
                        y: 140,
                        width: 120,
                        height: 40,
                    }),
                    actions: vec!["press".into()],
                    children: vec![],
                },
                RawNode {
                    handle: "btn-disabled".into(),
                    role: "button".into(),
                    name: "Locked".into(),
                    value: None,
                    app_id: Some("settings".into()),
                    pid: Some(500),
                    enabled: false,
                    focused: false,
                    bounds: Some(DesktopRect {
                        x: 180,
                        y: 140,
                        width: 120,
                        height: 40,
                    }),
                    actions: vec![],
                    children: vec![],
                },
            ],
        }]
    }

    #[test]
    fn node_ref_parsing_and_serialization_round_trip() {
        let r = NodeRef::parse("@p2:e14").unwrap();
        assert_eq!(r.snapshot.as_deref(), Some("p2"));
        assert_eq!(r.id, "e14");
        assert_eq!(r.to_string(), "@p2:e14");
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"@p2:e14\"");
        assert_eq!(serde_json::from_str::<NodeRef>(&json).unwrap(), r);

        let r2 = NodeRef::parse("e3").unwrap();
        assert_eq!(r2.snapshot, None);
        assert_eq!(r2.id, "e3");
        assert_eq!(r2.to_string(), "@e3");

        for bad in ["", "@", "@e", "@p1:x3", "button", "@bad snap:e1", "@p1:"] {
            assert_eq!(
                NodeRef::parse(bad).unwrap_err().code(),
                "invalid-ref",
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_chord_handles_modifiers_and_rejects_malformed() {
        let c = parse_chord("ctrl+shift+k").unwrap();
        assert!(c.ctrl && c.shift && !c.alt && !c.meta);
        assert_eq!(c.key, "k");
        let m = parse_chord("cmd+opt+Return").unwrap();
        assert!(m.meta && m.alt && !m.ctrl);
        assert_eq!(m.key, "Return");

        for bad in ["", "ctrl+", "+k", "ctrl", "a+b"] {
            assert_eq!(
                parse_chord(bad).unwrap_err().code(),
                "invalid-input",
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn mock_adapter_enforces_refs_rotation_bounds_actions_and_permissions() {
        let adapter = MockPlatformAdapter::with_roots(sample_raw_tree());

        // Action before any snapshot fails closed with StaleRef.
        let err = adapter
            .act(DesktopAction::Click {
                target: NodeRef::latest("e1"),
                button: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "stale-ref");

        // Snapshot 1 allocates deterministic @p1:eN refs.
        let t1 = adapter.snapshot(TreeOptions::default()).await.unwrap();
        assert_eq!(t1.snapshot_id, "p1");
        assert!(!t1.truncated);
        assert_eq!(t1.total_nodes, 4);
        assert_eq!(t1.find_node("@p1:e1").unwrap().name, "Settings");
        assert_eq!(t1.find_node("@p1:e2").unwrap().name, "Email");
        assert_eq!(t1.find_node("@p1:e3").unwrap().name, "Save");
        assert!(t1.text.contains("button \"Save\""));
        assert!(t1.text.contains("[@p1:e3]"));

        // Valid actions dispatch to exact handles.
        adapter
            .act(DesktopAction::SetValue {
                target: NodeRef::parse("@p1:e2").unwrap(),
                value: "new@example.com".into(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e3").unwrap(),
                button: Some(Button::Left),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::DoubleClick {
                target: NodeRef::parse("@e3").unwrap(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::Focus {
                target: NodeRef::parse("@p1:e1").unwrap(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::TypeText {
                text: "hello".into(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::PressKey {
                chord: "ctrl+s".into(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::Scroll {
                target: Some(NodeRef::parse("@p1:e1").unwrap()),
                dx: 0,
                dy: -3,
            })
            .await
            .unwrap();

        // Disabled node fails closed.
        let disabled = adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e4").unwrap(),
                button: None,
            })
            .await
            .unwrap_err();
        assert_eq!(disabled.code(), "unsupported-action");

        // Unknown ref in active snapshot fails with NodeNotFound.
        let missing = adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e99").unwrap(),
                button: None,
            })
            .await
            .unwrap_err();
        assert_eq!(missing.code(), "node-not-found");

        // Rotate snapshot: @p1:e3 must now be rejected with StaleRef!
        let t2 = adapter.snapshot(TreeOptions::default()).await.unwrap();
        assert_eq!(t2.snapshot_id, "p2");
        assert_eq!(
            t2.find_node("@p2:e2").unwrap().value.as_deref(),
            Some("new@example.com")
        );
        let stale = adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e3").unwrap(),
                button: None,
            })
            .await
            .unwrap_err();
        assert_eq!(stale.code(), "stale-ref");

        // Bounds & truncation: max_depth = 1 truncates children; max_nodes = 2 truncates siblings.
        let shallow = adapter
            .snapshot(TreeOptions {
                max_depth: 1,
                max_nodes: 100,
                pid: None,
            })
            .await
            .unwrap();
        assert!(shallow.truncated);
        assert_eq!(shallow.total_nodes, 1);
        assert!(shallow.text.contains("truncated"));

        let capped = adapter
            .snapshot(TreeOptions {
                max_depth: 10,
                max_nodes: 2,
                pid: None,
            })
            .await
            .unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.total_nodes, 2);

        // PermissionDenied fails closed on snapshot, act, and list_windows.
        adapter.set_permission_denied(Some("accessibility disabled".into()));
        assert_eq!(
            adapter
                .snapshot(TreeOptions::default())
                .await
                .unwrap_err()
                .code(),
            "permission-denied"
        );
        assert_eq!(
            adapter
                .act(DesktopAction::TypeText { text: "x".into() })
                .await
                .unwrap_err()
                .code(),
            "permission-denied"
        );
        assert_eq!(
            adapter.list_windows().await.unwrap_err().code(),
            "permission-denied"
        );

        // Recorded actions match expected handle mappings.
        let actions = adapter.recorded_actions();
        assert_eq!(actions.len(), 7);
        assert_eq!(actions[0].resolved_handle.as_deref(), Some("input-email"));
        assert_eq!(actions[1].resolved_handle.as_deref(), Some("btn-save"));
    }
}
