// Classify only shortcuts owned by the remote desktop. Plain keys and IME
// events must reach the usual keyboard handler unchanged.
const COMMANDS = new Set(['a', 'c', 'x', 'z', 'y', 'f', 'l', 'r', 'k', 's', 'b', 'i', 'u']);

export function desktopShortcut(e, mac) {
  if (e.isComposing || e.keyCode === 229 || e.getModifierState?.('AltGraph')) return null;
  const key = e.key.toLowerCase();
  const ctrl = e.ctrlKey && !e.metaKey && !e.altKey;
  const cmd = mac && e.metaKey && !e.ctrlKey && !e.altKey;
  if ((ctrl || cmd) && !e.shiftKey && key === 'v') {
    return { paste: true, read: mac && !!ctrl };
  }
  if (ctrl && !e.shiftKey && (key === 'c' || key === 'x')) return { copy: true };
  if (!mac) return null;
  // Do not let noVNC hold a Super or AltGr modifier during a translated chord.
  if (key === 'meta' || key === 'alt') return { suppress: true };
  if (cmd) {
    if (key === 'arrowleft') return { key: 'Home' };
    if (key === 'arrowright') return { key: 'End' };
    if (key === 'backspace') return { key: 'Backspace', ctrl: true };
    if (COMMANDS.has(key)) return { key, ctrl: true, copy: !e.shiftKey && (key === 'c' || key === 'x') };
  }
  if (e.altKey && !e.metaKey && !e.ctrlKey && (key === 'arrowleft' || key === 'arrowright')) {
    return { key: e.key, ctrl: true };
  }
  return null;
}

// iOS can honor an asynchronous focus without opening the system keyboard.
// That would leave the textarea focused and poison the next trusted tap.
export function requiresKeyboardGesture({ userAgent, platform, maxTouchPoints }) {
  return /iPhone|iPad|iPod/i.test(userAgent) ||
    (maxTouchPoints > 1 && (/Mac/i.test(platform) || /Macintosh/i.test(userAgent)));
}

const PURPOSES = new Set(['text', 'email', 'url', 'search', 'decimal']);
const inputRect = rect => Array.isArray(rect) && rect.length === 4 &&
  rect.every(Number.isFinite) && rect[2] > 0 && rect[3] > 0;

export function createSoftKeyboard({ coarse, canInput, asyncFocus, focused, focus, blur }) {
  let input = null, override = null;
  const show = () => { if (!focused()) focus(input?.purpose ?? 'text'); };
  const hide = () => { if (focused()) blur(); };

  return {
    update(state) {
      const wasActive = input?.active === true;
      input = typeof state?.active === 'boolean' && Array.isArray(state.rects) ? {
        active: state.active,
        purpose: PURPOSES.has(state.purpose) ? state.purpose : 'text',
        rects: state.rects.filter(inputRect),
      } : null;
      const active = input?.active === true;
      // Geometry updates and duplicate snapshots must not reopen an IME
      // that a miss just dismissed while remote focus was still active.
      if (!coarse() || !canInput() || override || active === wasActive) return;
      if (!active) hide();
      else if (asyncFocus()) show();
    },
    tap({ x, y }) {
      if (!coarse() || !canInput()) return;
      if (input?.rects.some(([rx, ry, w, h]) => x >= rx && x - rx < w && y >= ry && y - ry < h)) {
        override = null;
        show(); // synchronous with the trusted touchend
      } else if (override !== 'pin') hide();
    },
    toggle() {
      if (!canInput()) return;
      if (focused()) { override = 'suppress'; hide(); }
      else { override = 'pin'; show(); }
    },
    suspend: hide,
    reset() {
      input = override = null;
      hide();
    },
  };
}
