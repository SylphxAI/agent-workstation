//! macOS native host desktop backend: Accessibility (`AXUIElement`),
//! CoreFoundation, and CoreGraphics FFI in pure Rust.
//!
//! Zero external host scripts, zero screenshot pixel-matching, zero embedded
//! models. Fails closed with [`AdapterError::PermissionDenied`] whenever macOS
//! Accessibility permission (`AXIsProcessTrusted`) is not granted.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::desktop::{Button, DesktopRect, DesktopWindow};
use crate::platform::{
    AdapterError, DesktopAction, DesktopTree, PlatformAdapter, PlatformCapabilities, RawNode,
    SnapshotIndex, TreeOptions, parse_chord,
};

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_AX_ERROR_SUCCESS: i32 = 0;
const K_AX_ERROR_API_DISABLED: i32 = -25204;
const K_AX_VALUE_CGPOINT_TYPE: i32 = 1;
const K_AX_VALUE_CGSIZE_TYPE: i32 = 2;

const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;
const K_CG_MOUSE_EVENT_CLICK_STATE: u32 = 1;
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external: u8,
    ) -> *const c_void;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        s: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementCreateSystemWide() -> *const c_void;
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementGetPid(element: *const c_void, pid: *mut i32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *const c_void,
    ) -> i32;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: *const c_void, value_type: i32, value_ptr: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *const c_void;
    fn CGEventSetIntegerValueField(event: *const c_void, field: u32, value: i64);
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *const c_void;
    fn CGEventKeyboardSetUnicodeString(
        event: *const c_void,
        string_length: usize,
        unicode_string: *const u16,
    );
    fn CGEventSetFlags(event: *const c_void, flags: u64);
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *const c_void;
    fn CGEventPost(tap: u32, event: *const c_void);
}

/// RAII wrapper around an owned CoreFoundation / AX object reference.
struct CfRef(NonNull<c_void>);

// SAFETY: CoreFoundation reference counting (`CFRetain`/`CFRelease`) is thread-safe,
// and all `AXUIElement` operations are serialized by `MacOsPlatformAdapter`'s mutex.
unsafe impl Send for CfRef {}

impl CfRef {
    fn from_owned(ptr: *const c_void) -> Option<Self> {
        NonNull::new(ptr as *mut c_void).map(Self)
    }

    fn from_borrowed(ptr: *const c_void) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `ptr` is a non-null borrowed CoreFoundation object; retaining
        // transfers one owned reference that `Drop` will release.
        let retained = unsafe { CFRetain(ptr) };
        Self::from_owned(retained)
    }

    fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr().cast_const()
    }
}

impl Clone for CfRef {
    fn clone(&self) -> Self {
        // SAFETY: `self.0` is a valid owned CoreFoundation object.
        let retained = unsafe { CFRetain(self.as_ptr()) };
        Self(NonNull::new(retained as *mut c_void).expect("CFRetain returned null"))
    }
}

impl Drop for CfRef {
    fn drop(&mut self) {
        // SAFETY: `self.0` holds a balanced +1 CoreFoundation retain count.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

impl std::fmt::Debug for CfRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CfRef").field(&self.0).finish()
    }
}

/// Retained macOS `AXUIElement` handle stored inside [`SnapshotIndex`].
#[derive(Clone, Debug)]
pub struct AxHandle(CfRef);

/// Compile-time macOS [`PlatformAdapter`] backed by `AXUIElement` and `CGEvent`.
#[derive(Debug, Default)]
pub struct MacOsPlatformAdapter {
    index: Mutex<SnapshotIndex<AxHandle>>,
}

impl MacOsPlatformAdapter {
    /// Create a new macOS Accessibility adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Query macOS TCC Accessibility trust (`AXIsProcessTrusted`).
    pub fn is_trusted() -> bool {
        // SAFETY: `AXIsProcessTrusted` takes no arguments and returns a `Boolean`.
        unsafe { AXIsProcessTrusted() != 0 }
    }
}

fn require_trusted() -> Result<(), AdapterError> {
    if MacOsPlatformAdapter::is_trusted() {
        Ok(())
    } else {
        Err(AdapterError::PermissionDenied(
            "macOS Accessibility permission is not granted (AXIsProcessTrusted returned false)"
                .into(),
        ))
    }
}

fn cf_string(s: &str) -> Result<CfRef, AdapterError> {
    // SAFETY: `s.as_ptr()` is valid UTF-8 for `s.len()` bytes; allocator NULL uses default.
    let raw = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            s.as_ptr(),
            s.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    CfRef::from_owned(raw)
        .ok_or_else(|| AdapterError::Backend(format!("CFStringCreateWithBytes failed for {s:?}")))
}

fn cf_to_string(obj: &CfRef) -> Option<String> {
    // SAFETY: `obj` is a valid CoreFoundation object.
    unsafe {
        if CFGetTypeID(obj.as_ptr()) != CFStringGetTypeID() {
            return None;
        }
        let len = CFStringGetLength(obj.as_ptr());
        let max = CFStringGetMaximumSizeForEncoding(len, K_CF_STRING_ENCODING_UTF8) + 1;
        if max <= 0 {
            return Some(String::new());
        }
        let mut buf = vec![0 as c_char; max as usize];
        if CFStringGetCString(
            obj.as_ptr(),
            buf.as_mut_ptr(),
            max,
            K_CF_STRING_ENCODING_UTF8,
        ) == 0
        {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

fn cf_to_bool(obj: &CfRef) -> Option<bool> {
    // SAFETY: `obj` is a valid CoreFoundation object; type id is checked before reading.
    unsafe {
        if CFGetTypeID(obj.as_ptr()) != CFBooleanGetTypeID() {
            return None;
        }
        Some(CFBooleanGetValue(obj.as_ptr()) != 0)
    }
}

fn ax_copy_attr(elem: &CfRef, attr: &str) -> Result<Option<CfRef>, AdapterError> {
    let key = cf_string(attr)?;
    let mut out: *const c_void = ptr::null();
    // SAFETY: `elem` and `key` are valid CoreFoundation references and `out` is writable.
    let err = unsafe { AXUIElementCopyAttributeValue(elem.as_ptr(), key.as_ptr(), &mut out) };
    if err == K_AX_ERROR_API_DISABLED {
        return Err(AdapterError::PermissionDenied(
            "macOS Accessibility API is disabled (kAXErrorAPIDisabled)".into(),
        ));
    }
    if err != K_AX_ERROR_SUCCESS {
        return Ok(None);
    }
    Ok(CfRef::from_owned(out))
}

fn ax_bounds(elem: &CfRef) -> Option<DesktopRect> {
    let pos_val = ax_copy_attr(elem, "AXPosition").ok().flatten()?;
    let size_val = ax_copy_attr(elem, "AXSize").ok().flatten()?;
    let mut point = CGPoint::default();
    let mut size = CGSize::default();
    // SAFETY: Both `pos_val` and `size_val` are verified to have `AXValueGetTypeID()`
    // before unpacking matching `CGPoint` / `CGSize` structs.
    unsafe {
        let ax_val_tid = AXValueGetTypeID();
        if CFGetTypeID(pos_val.as_ptr()) != ax_val_tid
            || CFGetTypeID(size_val.as_ptr()) != ax_val_tid
        {
            return None;
        }
        if AXValueGetValue(
            pos_val.as_ptr(),
            K_AX_VALUE_CGPOINT_TYPE,
            (&mut point as *mut CGPoint).cast::<c_void>(),
        ) == 0
        {
            return None;
        }
        if AXValueGetValue(
            size_val.as_ptr(),
            K_AX_VALUE_CGSIZE_TYPE,
            (&mut size as *mut CGSize).cast::<c_void>(),
        ) == 0
        {
            return None;
        }
    }
    if !point.x.is_finite()
        || !point.y.is_finite()
        || !size.width.is_finite()
        || !size.height.is_finite()
        || size.width <= 0.0
        || size.height <= 0.0
    {
        return None;
    }
    Some(DesktopRect {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
        width: size.width.round() as u32,
        height: size.height.round() as u32,
    })
}

fn ax_pid(elem: &CfRef) -> Option<u32> {
    let mut pid: i32 = 0;
    // SAFETY: `elem` is a valid `AXUIElement` and `pid` is a valid writable `i32`.
    let err = unsafe { AXUIElementGetPid(elem.as_ptr(), &mut pid) };
    (err == K_AX_ERROR_SUCCESS && pid > 0).then_some(pid as u32)
}

fn ax_children(elem: &CfRef, attr: &str) -> Result<Vec<CfRef>, AdapterError> {
    let Some(arr) = ax_copy_attr(elem, attr)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    // SAFETY: `arr` type id is checked to equal `CFArrayGetTypeID()`, and each element
    // is checked against `AXUIElementGetTypeID()` before retaining.
    unsafe {
        if CFGetTypeID(arr.as_ptr()) != CFArrayGetTypeID() {
            return Ok(out);
        }
        let elem_tid = AXUIElementGetTypeID();
        let count = CFArrayGetCount(arr.as_ptr());
        for idx in 0..count {
            let item = CFArrayGetValueAtIndex(arr.as_ptr(), idx);
            if !item.is_null()
                && CFGetTypeID(item) == elem_tid
                && let Some(retained) = CfRef::from_borrowed(item)
            {
                out.push(retained);
            }
        }
    }
    Ok(out)
}

fn walk_ax(
    elem: CfRef,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
) -> Result<Option<RawNode<AxHandle>>, AdapterError> {
    if *budget == 0 {
        return Ok(None);
    }
    *budget = budget.saturating_sub(1);

    let role = ax_copy_attr(&elem, "AXRole")?
        .and_then(|r| cf_to_string(&r))
        .map(|s| normalize_ax_role(&s))
        .unwrap_or_else(|| "unknown".into());
    let name = ax_copy_attr(&elem, "AXTitle")?
        .and_then(|v| cf_to_string(&v))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ax_copy_attr(&elem, "AXDescription")
                .ok()
                .flatten()
                .and_then(|v| cf_to_string(&v))
        })
        .unwrap_or_default();
    let value = ax_copy_attr(&elem, "AXValue")?.and_then(|v| cf_to_string(&v));
    let enabled = ax_copy_attr(&elem, "AXEnabled")?
        .and_then(|v| cf_to_bool(&v))
        .unwrap_or(true);
    let focused = ax_copy_attr(&elem, "AXFocused")?
        .and_then(|v| cf_to_bool(&v))
        .unwrap_or(false);
    let bounds = ax_bounds(&elem);
    let pid = ax_pid(&elem);

    let mut children = Vec::new();
    if depth <= max_depth && *budget > 0 {
        let mut child_elems = ax_children(&elem, "AXChildren")?;
        if child_elems.is_empty() && role == "application" {
            child_elems = ax_children(&elem, "AXWindows")?;
        }
        for child in child_elems {
            if *budget == 0 {
                break;
            }
            if let Some(node) = walk_ax(child, depth + 1, max_depth, budget)? {
                children.push(node);
            }
        }
    }

    Ok(Some(RawNode {
        handle: AxHandle(elem),
        role,
        name,
        value,
        app_id: None,
        pid,
        enabled,
        focused,
        editable: false,
        checked: false,
        bounds,
        actions: Vec::new(),
        children,
    }))
}

fn normalize_ax_role(raw: &str) -> String {
    match raw {
        "AXApplication" => "application".into(),
        "AXWindow" => "window".into(),
        "AXButton" => "button".into(),
        "AXTextField" | "AXTextArea" => "text_field".into(),
        "AXStaticText" => "static_text".into(),
        "AXCheckBox" => "checkbox".into(),
        "AXMenuItem" => "menu_item".into(),
        "AXLink" => "link".into(),
        other => other
            .strip_prefix("AX")
            .unwrap_or(other)
            .to_ascii_lowercase(),
    }
}

fn post_mouse_click(pos: CGPoint, button: Button, click_count: i64) -> Result<(), AdapterError> {
    let (down_ty, up_ty, cg_btn) = match button {
        Button::Left => (
            K_CG_EVENT_LEFT_MOUSE_DOWN,
            K_CG_EVENT_LEFT_MOUSE_UP,
            K_CG_MOUSE_BUTTON_LEFT,
        ),
        Button::Right => (
            K_CG_EVENT_RIGHT_MOUSE_DOWN,
            K_CG_EVENT_RIGHT_MOUSE_UP,
            K_CG_MOUSE_BUTTON_RIGHT,
        ),
        Button::Middle | Button::Back | Button::Forward => (
            K_CG_EVENT_OTHER_MOUSE_DOWN,
            K_CG_EVENT_OTHER_MOUSE_UP,
            K_CG_MOUSE_BUTTON_CENTER,
        ),
    };
    for click_state in 1..=click_count {
        // SAFETY: CoreGraphics mouse events are owned CF objects released on Drop.
        let down = CfRef::from_owned(unsafe {
            CGEventCreateMouseEvent(ptr::null(), down_ty, pos, cg_btn)
        })
        .ok_or_else(|| AdapterError::Backend("CGEventCreateMouseEvent(down) failed".into()))?;
        let up =
            CfRef::from_owned(unsafe { CGEventCreateMouseEvent(ptr::null(), up_ty, pos, cg_btn) })
                .ok_or_else(|| {
                    AdapterError::Backend("CGEventCreateMouseEvent(up) failed".into())
                })?;
        unsafe {
            CGEventSetIntegerValueField(down.as_ptr(), K_CG_MOUSE_EVENT_CLICK_STATE, click_state);
            CGEventSetIntegerValueField(up.as_ptr(), K_CG_MOUSE_EVENT_CLICK_STATE, click_state);
            CGEventPost(K_CG_HID_EVENT_TAP, down.as_ptr());
            CGEventPost(K_CG_HID_EVENT_TAP, up.as_ptr());
        }
    }
    Ok(())
}

fn post_unicode_text(text: &str) -> Result<(), AdapterError> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    for chunk in utf16.chunks(20) {
        // SAFETY: `chunk` is a valid UTF-16 buffer for the duration of the call.
        let down =
            CfRef::from_owned(unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, true) })
                .ok_or_else(|| AdapterError::Backend("CGEventCreateKeyboardEvent failed".into()))?;
        let up = CfRef::from_owned(unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, false) })
            .ok_or_else(|| AdapterError::Backend("CGEventCreateKeyboardEvent failed".into()))?;
        unsafe {
            CGEventKeyboardSetUnicodeString(down.as_ptr(), chunk.len(), chunk.as_ptr());
            CGEventKeyboardSetUnicodeString(up.as_ptr(), chunk.len(), chunk.as_ptr());
            CGEventPost(K_CG_HID_EVENT_TAP, down.as_ptr());
            CGEventPost(K_CG_HID_EVENT_TAP, up.as_ptr());
        }
    }
    Ok(())
}

fn post_key_chord(chord: &str) -> Result<(), AdapterError> {
    let parsed = parse_chord(chord)?;
    let keycode = macos_keycode(&parsed.key).ok_or_else(|| {
        AdapterError::InvalidInput(format!("unsupported macOS key {:?}", parsed.key))
    })?;
    let mut flags = 0u64;
    if parsed.shift {
        flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
    }
    if parsed.ctrl {
        flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
    }
    if parsed.alt {
        flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
    }
    if parsed.meta {
        flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
    }
    // SAFETY: Owned CoreGraphics keyboard events posted and released via RAII.
    let down = CfRef::from_owned(unsafe { CGEventCreateKeyboardEvent(ptr::null(), keycode, true) })
        .ok_or_else(|| AdapterError::Backend("CGEventCreateKeyboardEvent(down) failed".into()))?;
    let up = CfRef::from_owned(unsafe { CGEventCreateKeyboardEvent(ptr::null(), keycode, false) })
        .ok_or_else(|| AdapterError::Backend("CGEventCreateKeyboardEvent(up) failed".into()))?;
    unsafe {
        if flags != 0 {
            CGEventSetFlags(down.as_ptr(), flags);
            CGEventSetFlags(up.as_ptr(), flags);
        }
        CGEventPost(K_CG_HID_EVENT_TAP, down.as_ptr());
        CGEventPost(K_CG_HID_EVENT_TAP, up.as_ptr());
    }
    Ok(())
}

fn post_scroll(pos: Option<CGPoint>, dx: i32, dy: i32) -> Result<(), AdapterError> {
    if let Some(p) = pos
        && let Some(mv) = CfRef::from_owned(unsafe {
            CGEventCreateMouseEvent(
                ptr::null(),
                K_CG_EVENT_MOUSE_MOVED,
                p,
                K_CG_MOUSE_BUTTON_LEFT,
            )
        })
    {
        // SAFETY: Move pointer over target bounds before scrolling.
        unsafe { CGEventPost(K_CG_HID_EVENT_TAP, mv.as_ptr()) };
    }
    // macOS scroll wheel convention: negative wheel1 scrolls down; negate `dy` to match positive=down.
    // SAFETY: Non-variadic `CGEventCreateScrollWheelEvent2` creates an owned CGEvent.
    let ev = CfRef::from_owned(unsafe {
        CGEventCreateScrollWheelEvent2(ptr::null(), K_CG_SCROLL_EVENT_UNIT_LINE, 2, -dy, -dx, 0)
    })
    .ok_or_else(|| AdapterError::Backend("CGEventCreateScrollWheelEvent2 failed".into()))?;
    unsafe { CGEventPost(K_CG_HID_EVENT_TAP, ev.as_ptr()) };
    Ok(())
}

/// Map common key names to macOS Carbon/CoreGraphics virtual keycodes.
pub(crate) fn macos_keycode(key: &str) -> Option<u16> {
    match key.to_ascii_lowercase().as_str() {
        "a" => Some(0x00),
        "s" => Some(0x01),
        "d" => Some(0x02),
        "f" => Some(0x03),
        "h" => Some(0x04),
        "g" => Some(0x05),
        "z" => Some(0x06),
        "x" => Some(0x07),
        "c" => Some(0x08),
        "v" => Some(0x09),
        "b" => Some(0x0b),
        "q" => Some(0x0c),
        "w" => Some(0x0d),
        "e" => Some(0x0e),
        "r" => Some(0x0f),
        "y" => Some(0x10),
        "t" => Some(0x11),
        "1" => Some(0x12),
        "2" => Some(0x13),
        "3" => Some(0x14),
        "4" => Some(0x15),
        "6" => Some(0x16),
        "5" => Some(0x17),
        "9" => Some(0x19),
        "7" => Some(0x1a),
        "8" => Some(0x1c),
        "0" => Some(0x1d),
        "o" => Some(0x1f),
        "u" => Some(0x20),
        "i" => Some(0x22),
        "p" => Some(0x23),
        "return" | "enter" => Some(0x24),
        "l" => Some(0x25),
        "j" => Some(0x26),
        "k" => Some(0x28),
        "n" => Some(0x2d),
        "m" => Some(0x2e),
        "tab" => Some(0x30),
        "space" => Some(0x31),
        "backspace" | "delete" => Some(0x33),
        "escape" | "esc" => Some(0x35),
        "left" | "arrowleft" => Some(0x7b),
        "right" | "arrowright" => Some(0x7c),
        "down" | "arrowdown" => Some(0x7d),
        "up" | "arrowup" => Some(0x7e),
        _ => None,
    }
}

#[async_trait]
impl PlatformAdapter for MacOsPlatformAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        let trusted = Self::is_trusted();
        PlatformCapabilities {
            platform: "macos".into(),
            backend: "macos-ax".into(),
            accessibility_tree: trusted,
            native_actions: trusted,
            keyboard_injection: trusted,
            pointer_injection: trusted,
            window_enumeration: trusted,
        }
    }

    async fn snapshot(&self, options: TreeOptions) -> Result<DesktopTree, AdapterError> {
        require_trusted()?;
        let (max_depth, max_nodes) = options.effective_bounds();
        let root_elem = if let Some(pid) = options.pid {
            // SAFETY: Creates an owned `AXUIElement` for `pid`.
            CfRef::from_owned(unsafe { AXUIElementCreateApplication(pid as i32) }).ok_or_else(
                || AdapterError::Backend(format!("AXUIElementCreateApplication({pid}) failed")),
            )?
        } else {
            // SAFETY: Creates an owned system-wide `AXUIElement`.
            let sys =
                CfRef::from_owned(unsafe { AXUIElementCreateSystemWide() }).ok_or_else(|| {
                    AdapterError::Backend("AXUIElementCreateSystemWide failed".into())
                })?;
            ax_copy_attr(&sys, "AXFocusedApplication")?
                .ok_or_else(|| AdapterError::Backend("no focused macOS application".into()))?
        };
        let mut budget = max_nodes.saturating_add(1);
        let mut raw_roots = Vec::new();
        if let Some(root) = walk_ax(root_elem, 1, max_depth, &mut budget)? {
            raw_roots.push(root);
        }
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        Ok(index.build("macos", raw_roots, options))
    }

    async fn act(&self, action: DesktopAction) -> Result<(), AdapterError> {
        require_trusted()?;
        match action {
            DesktopAction::Click { target, button } => {
                let (node, handle) = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, h) = idx.resolve(&target)?;
                    (n.clone(), h.clone())
                };
                if button.unwrap_or(Button::Left) == Button::Left {
                    let press = cf_string("AXPress")?;
                    // SAFETY: Both `handle.0` and `press` are valid CF references.
                    let err =
                        unsafe { AXUIElementPerformAction(handle.0.as_ptr(), press.as_ptr()) };
                    if err == K_AX_ERROR_SUCCESS {
                        return Ok(());
                    }
                    if err == K_AX_ERROR_API_DISABLED {
                        return Err(AdapterError::PermissionDenied(
                            "macOS Accessibility API disabled".into(),
                        ));
                    }
                }
                let bounds = node.bounds.ok_or_else(|| {
                    AdapterError::UnsupportedAction(format!(
                        "node {target} did not accept AXPress and has no screen bounds"
                    ))
                })?;
                let (x, y) = bounds.center();
                post_mouse_click(CGPoint { x, y }, button.unwrap_or(Button::Left), 1)
            }
            DesktopAction::DoubleClick { target } => {
                let bounds = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, _) = idx.resolve(&target)?;
                    n.bounds.ok_or_else(|| {
                        AdapterError::UnsupportedAction(format!(
                            "node {target} has no screen bounds for double-click"
                        ))
                    })?
                };
                let (x, y) = bounds.center();
                post_mouse_click(CGPoint { x, y }, Button::Left, 2)
            }
            DesktopAction::Focus { target } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                let raise = cf_string("AXRaise")?;
                let focused_attr = cf_string("AXFocused")?;
                // SAFETY: Valid CF references; `kCFBooleanTrue` is an immutable system singleton.
                unsafe {
                    let _ = AXUIElementPerformAction(handle.0.as_ptr(), raise.as_ptr());
                    let err = AXUIElementSetAttributeValue(
                        handle.0.as_ptr(),
                        focused_attr.as_ptr(),
                        kCFBooleanTrue,
                    );
                    if err == K_AX_ERROR_API_DISABLED {
                        return Err(AdapterError::PermissionDenied(
                            "macOS Accessibility API disabled".into(),
                        ));
                    }
                }
                Ok(())
            }
            DesktopAction::SetValue { target, value } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                let attr = cf_string("AXValue")?;
                let val = cf_string(&value)?;
                // SAFETY: `handle.0`, `attr`, and `val` are valid CF references.
                let err = unsafe {
                    AXUIElementSetAttributeValue(handle.0.as_ptr(), attr.as_ptr(), val.as_ptr())
                };
                if err == K_AX_ERROR_SUCCESS {
                    Ok(())
                } else if err == K_AX_ERROR_API_DISABLED {
                    Err(AdapterError::PermissionDenied(
                        "macOS Accessibility API disabled".into(),
                    ))
                } else {
                    Err(AdapterError::UnsupportedAction(format!(
                        "AXUIElementSetAttributeValue(AXValue) failed with AXError {err}"
                    )))
                }
            }
            DesktopAction::TypeText { text } => post_unicode_text(&text),
            DesktopAction::PressKey { chord } => post_key_chord(&chord),
            DesktopAction::Scroll { target, dx, dy } => {
                let pos = match target {
                    Some(t) => {
                        let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                        let (n, _) = idx.resolve(&t)?;
                        n.bounds.map(|b| {
                            let (x, y) = b.center();
                            CGPoint { x, y }
                        })
                    }
                    None => None,
                };
                post_scroll(pos, dx, dy)
            }
        }
    }

    async fn list_windows(&self) -> Result<Vec<DesktopWindow>, AdapterError> {
        require_trusted()?;
        // SAFETY: Creates an owned system-wide `AXUIElement`.
        let sys = CfRef::from_owned(unsafe { AXUIElementCreateSystemWide() })
            .ok_or_else(|| AdapterError::Backend("AXUIElementCreateSystemWide failed".into()))?;
        let Some(app) = ax_copy_attr(&sys, "AXFocusedApplication")? else {
            return Ok(Vec::new());
        };
        let app_name = ax_copy_attr(&app, "AXTitle")?
            .and_then(|v| cf_to_string(&v))
            .filter(|s| !s.is_empty());
        let pid = ax_pid(&app);
        let windows = ax_children(&app, "AXWindows")?;
        let mut out = Vec::with_capacity(windows.len());
        for (idx, w) in windows.into_iter().enumerate() {
            let name = ax_copy_attr(&w, "AXTitle")?
                .and_then(|v| cf_to_string(&v))
                .unwrap_or_default();
            let focused = ax_copy_attr(&w, "AXMain")?
                .and_then(|v| cf_to_bool(&v))
                .unwrap_or(false);
            let rect = ax_bounds(&w).unwrap_or_default();
            out.push(DesktopWindow {
                id: (idx + 1) as u64,
                name,
                app_id: app_name.clone(),
                pid,
                focused,
                rect,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_capabilities_and_keycodes_are_well_defined() {
        let adapter = MacOsPlatformAdapter::new();
        let caps = adapter.capabilities();
        assert_eq!(caps.platform, "macos");
        assert_eq!(caps.backend, "macos-ax");
        assert_eq!(macos_keycode("a"), Some(0x00));
        assert_eq!(macos_keycode("Return"), Some(0x24));
        assert_eq!(macos_keycode("Escape"), Some(0x35));
        assert_eq!(macos_keycode("NotAKey"), None);
    }

    #[tokio::test]
    async fn untrusted_process_fails_closed_with_permission_denied() {
        if MacOsPlatformAdapter::is_trusted() {
            return;
        }
        let adapter = MacOsPlatformAdapter::new();
        let err = adapter.snapshot(TreeOptions::default()).await.unwrap_err();
        assert_eq!(err.code(), "permission-denied");
        let err_w = adapter.list_windows().await.unwrap_err();
        assert_eq!(err_w.code(), "permission-denied");
    }
}
