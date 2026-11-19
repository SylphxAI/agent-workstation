//! Windows native host desktop backend: UI Automation (`IUIAutomation` COM)
//! and Win32 input (`SendInput`, `SetCursorPos`, `SetForegroundWindow`) in pure Rust.
//!
//! Zero `.ps1` host scripts, zero screenshot pixel-matching, zero embedded models.

#![allow(unsafe_code)]

use std::sync::Mutex;

use async_trait::async_trait;
use windows::Win32::Foundation::{E_ACCESSDENIED, HWND, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_ButtonControlTypeId,
    UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId, UIA_EditControlTypeId,
    UIA_HyperlinkControlTypeId, UIA_InvokePatternId, UIA_ListItemControlTypeId,
    UIA_MenuItemControlTypeId, UIA_TextControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN,
    VK_ESCAPE, VK_LEFT, VK_LWIN, VK_MENU, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{SetCursorPos, SetForegroundWindow};
use windows::core::BSTR;

use crate::desktop::{Button, DesktopRect, DesktopWindow};
use crate::platform::{
    AdapterError, DesktopAction, DesktopTree, PlatformAdapter, PlatformCapabilities, RawNode,
    SnapshotIndex, TreeOptions, parse_chord,
};

const WHEEL_DELTA: i32 = 120;

/// RAII COM multi-threaded apartment initialization guard for the current thread.
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_mta() -> Result<Self, AdapterError> {
        // SAFETY: Initializes COM MTA on the current worker thread; paired with `CoUninitialize` on Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            // Thread already initialized COM in another apartment mode; do not uninitialize.
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(map_hresult_err(
                windows::core::Error::from_hresult(hr),
                "CoInitializeEx",
            ))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Balanced with `CoInitializeEx` returning S_OK / S_FALSE on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Handle to a UI Automation element created inside the COM Multi-Threaded Apartment.
#[derive(Clone, Debug)]
pub struct UiaHandle {
    element: IUIAutomationElement,
    hwnd: isize,
}

// SAFETY: Every `IUIAutomationElement` stored here is created via an MTA `CUIAutomation`
// instance and accessed only under `WindowsPlatformAdapter`'s mutex from MTA-initialized threads.
unsafe impl Send for UiaHandle {}

/// Compile-time Windows [`PlatformAdapter`] backed by UI Automation (`IUIAutomation`).
#[derive(Debug, Default)]
pub struct WindowsPlatformAdapter {
    index: Mutex<SnapshotIndex<UiaHandle>>,
}

impl WindowsPlatformAdapter {
    /// Create a new Windows UI Automation adapter.
    pub fn new() -> Self {
        Self::default()
    }
}

fn map_hresult_err(err: windows::core::Error, context: &str) -> AdapterError {
    if err.code() == E_ACCESSDENIED {
        AdapterError::PermissionDenied(format!("{context}: access denied ({err})"))
    } else {
        AdapterError::Backend(format!("{context}: {err}"))
    }
}

fn create_uia() -> Result<IUIAutomation, AdapterError> {
    // SAFETY: Creates the standard in-process `CUIAutomation` COM server on an MTA thread.
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| map_hresult_err(e, "CoCreateInstance(CUIAutomation)"))
}

fn normalize_uia_role(control_type: UIA_CONTROLTYPE_ID) -> String {
    match control_type {
        UIA_WindowControlTypeId => "window".into(),
        UIA_ButtonControlTypeId => "button".into(),
        UIA_EditControlTypeId => "text_field".into(),
        UIA_TextControlTypeId => "static_text".into(),
        UIA_CheckBoxControlTypeId => "checkbox".into(),
        UIA_MenuItemControlTypeId => "menu_item".into(),
        UIA_HyperlinkControlTypeId => "link".into(),
        UIA_ListItemControlTypeId => "list_item".into(),
        other => format!("uia_{}", other.0),
    }
}

fn rect_to_desktop(r: RECT) -> Option<DesktopRect> {
    let w = r.right.saturating_sub(r.left);
    let h = r.bottom.saturating_sub(r.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(DesktopRect {
        x: r.left,
        y: r.top,
        width: w as u32,
        height: h as u32,
    })
}

fn walk_uia(
    walker: &IUIAutomationTreeWalker,
    elem: IUIAutomationElement,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    pid_filter: Option<u32>,
) -> Option<RawNode<UiaHandle>> {
    if *budget == 0 {
        return None;
    }
    // SAFETY: Standard UIA property getters on a live `IUIAutomationElement`.
    let pid = unsafe { elem.CurrentProcessId() }
        .ok()
        .and_then(|p| u32::try_from(p).ok());
    if depth == 1 && pid_filter.is_some() && pid != pid_filter {
        return None;
    }
    *budget = budget.saturating_sub(1);

    let role = unsafe { elem.CurrentControlType() }
        .map(normalize_uia_role)
        .unwrap_or_else(|_| "unknown".into());
    let name = unsafe { elem.CurrentName() }
        .map(|s| s.to_string())
        .unwrap_or_default();
    let value =
        unsafe { elem.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()
            .and_then(|pat| unsafe { pat.CurrentValue() }.ok())
            .map(|v| v.to_string());
    let enabled = unsafe { elem.CurrentIsEnabled() }
        .map(|b| b.as_bool())
        .unwrap_or(true);
    let focused = unsafe { elem.CurrentHasKeyboardFocus() }
        .map(|b| b.as_bool())
        .unwrap_or(false);
    let bounds = unsafe { elem.CurrentBoundingRectangle() }
        .ok()
        .and_then(rect_to_desktop);
    let hwnd = unsafe { elem.CurrentNativeWindowHandle() }
        .map(|h| h.0 as isize)
        .unwrap_or(0);

    let mut children = Vec::new();
    if depth <= max_depth
        && *budget > 0
        && let Ok(mut child) = unsafe { walker.GetFirstChildElement(&elem) }
    {
        loop {
            if *budget == 0 {
                break;
            }
            let next = unsafe { walker.GetNextSiblingElement(&child) }.ok();
            if let Some(node) = walk_uia(walker, child, depth + 1, max_depth, budget, None) {
                children.push(node);
            }
            match next {
                Some(n) => child = n,
                None => break,
            }
        }
    }

    Some(RawNode {
        handle: UiaHandle {
            element: elem,
            hwnd,
        },
        role,
        name,
        value,
        app_id: None,
        pid,
        enabled,
        focused,
        bounds,
        actions: Vec::new(),
        children,
    })
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), AdapterError> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` slice is valid for `inputs.len()` elements of exact `size_of::<INPUT>()`.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(AdapterError::PermissionDenied(
            "Win32 SendInput was blocked (UIPI or permission denied)".into(),
        ));
    }
    Ok(())
}

fn mouse_click_at(x: i32, y: i32, button: Button, count: usize) -> Result<(), AdapterError> {
    // SAFETY: Moves cursor to screen coordinates before sending button events.
    unsafe { SetCursorPos(x, y) }.map_err(|e| map_hresult_err(e, "SetCursorPos"))?;
    let (down_flag, up_flag) = match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Button::Middle | Button::Back | Button::Forward => {
            (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)
        }
    };
    let mut seq = Vec::with_capacity(count * 2);
    for _ in 0..count {
        seq.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: down_flag,
                    ..Default::default()
                },
            },
        });
        seq.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: up_flag,
                    ..Default::default()
                },
            },
        });
    }
    send_inputs(&seq)
}

fn send_unicode_text(text: &str) -> Result<(), AdapterError> {
    let mut seq = Vec::new();
    for unit in text.encode_utf16() {
        seq.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    ..Default::default()
                },
            },
        });
        seq.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    ..Default::default()
                },
            },
        });
    }
    send_inputs(&seq)
}

fn send_key_chord(chord: &str) -> Result<(), AdapterError> {
    let parsed = parse_chord(chord)?;
    let vk = windows_vk(&parsed.key).ok_or_else(|| {
        AdapterError::InvalidInput(format!("unsupported Windows key {:?}", parsed.key))
    })?;
    let mut mods = Vec::new();
    if parsed.ctrl {
        mods.push(VK_CONTROL);
    }
    if parsed.shift {
        mods.push(VK_SHIFT);
    }
    if parsed.alt {
        mods.push(VK_MENU);
    }
    if parsed.meta {
        mods.push(VK_LWIN);
    }
    let key_input = |key: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let mut seq = Vec::new();
    for &m in &mods {
        seq.push(key_input(m, KEYBD_EVENT_FLAGS(0)));
    }
    seq.push(key_input(vk, KEYBD_EVENT_FLAGS(0)));
    seq.push(key_input(vk, KEYEVENTF_KEYUP));
    for &m in mods.iter().rev() {
        seq.push(key_input(m, KEYEVENTF_KEYUP));
    }
    send_inputs(&seq)
}

/// Map common key names to Win32 `VIRTUAL_KEY` codes.
pub(crate) fn windows_vk(key: &str) -> Option<VIRTUAL_KEY> {
    let lower = key.to_ascii_lowercase();
    if lower.len() == 1 {
        let ch = lower.as_bytes()[0];
        if ch.is_ascii_lowercase() {
            return Some(VIRTUAL_KEY(u16::from(ch.to_ascii_uppercase())));
        }
        if ch.is_ascii_digit() {
            return Some(VIRTUAL_KEY(u16::from(ch)));
        }
    }
    match lower.as_str() {
        "return" | "enter" => Some(VK_RETURN),
        "tab" => Some(VK_TAB),
        "space" => Some(VK_SPACE),
        "backspace" => Some(VK_BACK),
        "delete" => Some(VK_DELETE),
        "escape" | "esc" => Some(VK_ESCAPE),
        "left" | "arrowleft" => Some(VK_LEFT),
        "right" | "arrowright" => Some(VK_RIGHT),
        "up" | "arrowup" => Some(VK_UP),
        "down" | "arrowdown" => Some(VK_DOWN),
        _ => None,
    }
}

#[async_trait]
impl PlatformAdapter for WindowsPlatformAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            platform: "windows".into(),
            backend: "windows-uia".into(),
            accessibility_tree: true,
            native_actions: true,
            keyboard_injection: true,
            pointer_injection: true,
            window_enumeration: true,
        }
    }

    async fn snapshot(&self, options: TreeOptions) -> Result<DesktopTree, AdapterError> {
        let _com = ComGuard::init_mta()?;
        let uia = create_uia()?;
        let (max_depth, max_nodes) = options.effective_bounds();
        // SAFETY: Standard UIA desktop root and control-view tree walker creation.
        let root = unsafe { uia.GetRootElement() }
            .map_err(|e| map_hresult_err(e, "IUIAutomation::GetRootElement"))?;
        let walker = unsafe { uia.ControlViewWalker() }
            .map_err(|e| map_hresult_err(e, "IUIAutomation::ControlViewWalker"))?;

        let mut budget = max_nodes.saturating_add(1);
        let mut raw_roots = Vec::new();
        if let Ok(mut child) = unsafe { walker.GetFirstChildElement(&root) } {
            loop {
                if budget == 0 {
                    break;
                }
                let next = unsafe { walker.GetNextSiblingElement(&child) }.ok();
                if let Some(node) = walk_uia(&walker, child, 1, max_depth, &mut budget, options.pid)
                {
                    raw_roots.push(node);
                }
                match next {
                    Some(n) => child = n,
                    None => break,
                }
            }
        }
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        Ok(index.build("windows", raw_roots, options))
    }

    async fn act(&self, action: DesktopAction) -> Result<(), AdapterError> {
        let _com = ComGuard::init_mta()?;
        match action {
            DesktopAction::Click { target, button } => {
                let (node, handle) = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, h) = idx.resolve(&target)?;
                    (n.clone(), h.clone())
                };
                if button.unwrap_or(Button::Left) == Button::Left
                    && let Ok(invoke) = unsafe {
                        handle
                            .element
                            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    }
                    && unsafe { invoke.Invoke() }.is_ok()
                {
                    return Ok(());
                }
                let bounds = node.bounds.ok_or_else(|| {
                    AdapterError::UnsupportedAction(format!(
                        "node {target} does not support InvokePattern and has no screen bounds"
                    ))
                })?;
                let (x, y) = bounds.center();
                mouse_click_at(
                    x.round() as i32,
                    y.round() as i32,
                    button.unwrap_or(Button::Left),
                    1,
                )
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
                mouse_click_at(x.round() as i32, y.round() as i32, Button::Left, 2)
            }
            DesktopAction::Focus { target } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                if handle.hwnd != 0 {
                    // SAFETY: `handle.hwnd` came from `CurrentNativeWindowHandle`.
                    let _ =
                        unsafe { SetForegroundWindow(HWND(handle.hwnd as *mut core::ffi::c_void)) };
                }
                unsafe { handle.element.SetFocus() }
                    .map_err(|e| map_hresult_err(e, "IUIAutomationElement::SetFocus"))
            }
            DesktopAction::SetValue { target, value } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                let pat = unsafe {
                    handle
                        .element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                }
                .map_err(|e| {
                    AdapterError::UnsupportedAction(format!(
                        "node {target} does not support ValuePattern: {e}"
                    ))
                })?;
                unsafe { pat.SetValue(&BSTR::from(value)) }
                    .map_err(|e| map_hresult_err(e, "IUIAutomationValuePattern::SetValue"))
            }
            DesktopAction::TypeText { text } => send_unicode_text(&text),
            DesktopAction::PressKey { chord } => send_key_chord(&chord),
            DesktopAction::Scroll { target, dx, dy } => {
                if let Some(t) = target {
                    let bounds = {
                        let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                        idx.resolve(&t)?.0.bounds
                    };
                    if let Some(b) = bounds {
                        let (x, y) = b.center();
                        unsafe { SetCursorPos(x.round() as i32, y.round() as i32) }
                            .map_err(|e| map_hresult_err(e, "SetCursorPos"))?;
                    }
                }
                let mut seq = Vec::new();
                if dy != 0 {
                    seq.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                mouseData: (-dy * WHEEL_DELTA) as u32,
                                dwFlags: MOUSEEVENTF_WHEEL,
                                ..Default::default()
                            },
                        },
                    });
                }
                if dx != 0 {
                    seq.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                mouseData: (dx * WHEEL_DELTA) as u32,
                                dwFlags: MOUSEEVENTF_HWHEEL,
                                ..Default::default()
                            },
                        },
                    });
                }
                send_inputs(&seq)
            }
        }
    }

    async fn list_windows(&self) -> Result<Vec<DesktopWindow>, AdapterError> {
        let _com = ComGuard::init_mta()?;
        let uia = create_uia()?;
        let root = unsafe { uia.GetRootElement() }
            .map_err(|e| map_hresult_err(e, "IUIAutomation::GetRootElement"))?;
        let walker = unsafe { uia.ControlViewWalker() }
            .map_err(|e| map_hresult_err(e, "IUIAutomation::ControlViewWalker"))?;

        let mut windows = Vec::new();
        let mut idx = 0u64;
        if let Ok(mut child) = unsafe { walker.GetFirstChildElement(&root) } {
            loop {
                idx += 1;
                let name = unsafe { child.CurrentName() }
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let pid = unsafe { child.CurrentProcessId() }
                    .ok()
                    .and_then(|p| u32::try_from(p).ok());
                let focused = unsafe { child.CurrentHasKeyboardFocus() }
                    .map(|b| b.as_bool())
                    .unwrap_or(false);
                let rect = unsafe { child.CurrentBoundingRectangle() }
                    .ok()
                    .and_then(rect_to_desktop)
                    .unwrap_or_default();
                let hwnd = unsafe { child.CurrentNativeWindowHandle() }
                    .map(|h| h.0 as usize as u64)
                    .unwrap_or(idx);
                windows.push(DesktopWindow {
                    id: if hwnd != 0 { hwnd } else { idx },
                    name,
                    app_id: None,
                    pid,
                    focused,
                    rect,
                });
                match unsafe { walker.GetNextSiblingElement(&child) } {
                    Ok(next) => child = next,
                    Err(_) => break,
                }
            }
        }
        Ok(windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_capabilities_and_vk_mapping() {
        let adapter = WindowsPlatformAdapter::new();
        let caps = adapter.capabilities();
        assert_eq!(caps.platform, "windows");
        assert_eq!(caps.backend, "windows-uia");
        assert!(caps.accessibility_tree);
        assert_eq!(windows_vk("a"), Some(VIRTUAL_KEY(b'A' as u16)));
        assert_eq!(windows_vk("Return"), Some(VK_RETURN));
        assert_eq!(windows_vk("Escape"), Some(VK_ESCAPE));
        assert_eq!(windows_vk("NotAKey"), None);
    }

    #[tokio::test]
    async fn windows_uia_snapshot_executes_cleanly() {
        let adapter = WindowsPlatformAdapter::new();
        match adapter
            .snapshot(TreeOptions {
                max_depth: 2,
                max_nodes: 32,
                pid: None,
            })
            .await
        {
            Ok(tree) => {
                assert_eq!(tree.platform, "windows");
                assert_eq!(tree.snapshot_id, "p1");
            }
            Err(AdapterError::PermissionDenied(_)) => {}
            Err(other) => panic!("unexpected Windows UIA error: {other}"),
        }
    }
}
