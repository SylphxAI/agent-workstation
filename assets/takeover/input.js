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
