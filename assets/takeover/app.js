// Takeover page: the desktop over noVNC, gated by who holds control.
import { createSoftKeyboard, desktopShortcut, requiresKeyboardGesture } from './input.js';
import { createViewport } from './viewport.js';
import RFB from './novnc/core/rfb.js';
import KeyTable from './novnc/core/input/keysym.js';
import keysyms from './novnc/core/input/keysymdef.js';

const $ = id => document.getElementById(id);
// The token now lives in an HttpOnly cookie; keep it out of history and screenshots.
history.replaceState(null, '', location.pathname);

const WARN_MS = 30_000;
const CONNECT_MS = 8_000;
let browserRunning = true;
const mac = /mac/i.test(navigator.userAgentData?.platform || navigator.platform);
const modalOpen = () => !!document.querySelector('dialog[open]');
let rfb = null, connected = false, holder = 'agent', since = null, deadline = 0;
let retryTimer, failures = 0, reconnecting = false;
let link = 'Connecting…', latencyProbe = null;
let waylandClipboard = false, remoteClip = '', clipSource = null, generation = 0;
let feedbackTimer, pendingCopy, clipDialog, releasing = false;
let pasteQueue = Promise.resolve();
const canInput = () => connected && holder === 'human' && !releasing && !modalOpen();
const canSync = () => canInput() && !document.hidden && document.hasFocus();
const sticky = new Set();
const ime = $('ime');
const coarsePointer = matchMedia('(pointer: coarse)');
const gestureRequired = requiresKeyboardGesture(navigator);
const keyboard = createSoftKeyboard({
  coarse: () => coarsePointer.matches,
  canInput,
  asyncFocus: () => !gestureRequired && !document.hidden && document.hasFocus(),
  focused: () => document.activeElement === ime,
  focus: purpose => { ime.inputMode = purpose; ime.focus({ preventScroll: true }); },
  blur: () => ime.blur(),
});

const viewport = createViewport({
  screen: $('screen'), frame: $('frame'), cursor: $('cursor'), enabled: canInput,
  send: (x, y, mask) => rfb?.sendPointer(x, y, mask),
  onTap: keyboard.tap,
  focus: () => { if (document.activeElement !== $('ime')) rfb?.focus({ preventScroll: true }); },
  changed: ({ scale, fitted }) => {
    $('zoom').textContent = Math.round(scale * 100) + '%';
    $('fit').textContent = fitted ? 'Focus 100%' : 'Fit';
  },
});

function showLink() {
  const label = !browserRunning ? 'Browser stopped' : connected
    ? (holder === 'human' ? 'Live · In control' : 'Live · View only') : link;
  if ($('connection-label').textContent !== label) $('connection-label').textContent = label;
  $('signal').dataset.state = !browserRunning || !connected ? 'waiting' : holder === 'human' ? 'control' : 'view';
  $('reconnect').disabled = reconnecting;
}

// Measure the authenticated gateway round trip, not framebuffer/video latency.
async function measureLatency() {
  if (!connected || document.hidden || latencyProbe) return;
  const start = performance.now(), probe = new AbortController();
  latencyProbe = probe;
  const timeout = setTimeout(() => probe.abort(), 4000);
  try {
    const res = await fetch('state', { method: 'HEAD', cache: 'no-store', signal: probe.signal });
    if (!res.ok) throw new Error('Gateway unavailable');
    if (latencyProbe === probe) $('latency').textContent = Math.round(performance.now() - start) + ' ms';
  } catch {
    if (latencyProbe === probe) $('latency').textContent = 'RTT unavailable';
  } finally {
    clearTimeout(timeout);
    if (latencyProbe === probe) latencyProbe = null;
  }
}

function scheduleRetry(reason) {
  const delay = Math.min(2 ** ++failures, 16);
  link = reason + ' · Retry in ' + delay + 's';
  retryTimer = setTimeout(connect, delay * 1000);
  render();
}

function connect() {
  clearTimeout(retryTimer);
  if (rfb) return;
  link = failures ? 'Reconnecting…' : 'Connecting…';
  showLink();
  const url = new URL('ws', location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  // The viewport owns pointer input; noVNC updates its native CSS cursor.
  let connection;
  try {
    connection = new RFB($('frame'), url.href, { wsProtocols: ['binary'], cursorTarget: $('screen') });
  } catch {
    scheduleRetry('Could not connect');
    return;
  }
  rfb = connection;
  connection.viewOnly = !canInput();
  let canvas, timedOut = false;
  const handshake = setTimeout(() => {
    if (reconnecting) return;
    timedOut = true;
    link = 'Connection timed out';
    showLink();
    connection.disconnect();
  }, CONNECT_MS);
  const observer = new ResizeObserver(() => {
    if (connected && rfb === connection) viewport.framebuffer(canvas.width, canvas.height);
  });
  connection.addEventListener('connect', () => {
    clearTimeout(handshake);
    canvas = $('frame').querySelector('canvas');
    observer.observe(canvas);
    failures = 0;
    connected = true;
    viewport.framebuffer(canvas.width, canvas.height);
    render();
    // The server deduplicates input snapshots. Request a fresh baseline for
    // this framebuffer instead of reusing geometry from a previous socket.
    watchEvents();
    measureLatency();
  });
  connection.addEventListener('disconnect', () => {
    clearTimeout(handshake);
    observer.disconnect();
    if (rfb !== connection) return;
    connected = false;
    rfb = null;
    keyboard.reset();
    viewport.cancel();
    latencyProbe?.abort();
    latencyProbe = null;
    $('latency').textContent = '—';
    if ($('clip-dlg').open) $('clip-dlg').close('cancel');
    if (reconnecting) {
      reconnecting = false;
      connect();
      render();
    } else {
      scheduleRetry(timedOut ? 'Timed out' : 'Disconnected');
    }
  });
  connection.addEventListener('clipboard', e => {
    if (rfb === connection && !waylandClipboard && canSync()) receiveClipboard(e.detail.text);
  });
}

$('reconnect').onclick = () => {
  clearTimeout(retryTimer);
  failures = 0;
  if (!rfb) { connect(); return; }
  // Only replace this WebSocket. Custody and the remote desktop are unchanged.
  viewport.cancel();
  reconnecting = true;
  connected = false;
  keyboard.reset();
  $('latency').textContent = '—';
  latencyProbe?.abort();
  latencyProbe = null;
  link = 'Reconnecting…';
  render();
  rfb.disconnect();
};
setInterval(measureLatency, 8000);
document.addEventListener('visibilitychange', measureLatency);

function render() {
  const human = holder === 'human';
  $('browser-exited').hidden = browserRunning;
  $('browser-exited-human').hidden = !human;
  $('browser-exited-agent').hidden = human;
  const status = !browserRunning ? 'The browser has stopped.' : !connected ? 'Desktop disconnected, reconnecting…'
    : human ? 'You are in control. The agent is paused.' : 'The agent is in control (view only).';
  if ($('status').textContent !== status) $('status').textContent = status;
  showLink();
  $('take').hidden = human;
  $('take').disabled = !browserRunning || !connected;
  $('handback').hidden = !human;
  document.querySelectorAll('.needs-control, #keys button').forEach(b => { b.disabled = !canInput(); });
  syncClipboard();
  updateCopy();
  for (const id of ['kbd', 'keys-btn', 'clip-btn']) $(id).hidden = !human;
  if (!canInput()) {
    if (activeTray !== 'tools') setTray(null);
    keyboard.suspend();
    clearSticky();
  }
  if (rfb && rfb.viewOnly === canInput()) rfb.viewOnly = !canInput();
  $('screen').dataset.viewOnly = String(!canInput());
  viewport.paintCursor();
  tickIdle();
}

function apply(state) {
  const next = state.since ?? null;
  const cycleChanged = next !== since || state.holder !== holder;
  if (cycleChanged && holder === 'human') viewport.cancel();
  if (cycleChanged) {
    resetClipboard();
    // A stale local dialog must not be able to commit in a new cycle.
    for (const id of ['clip-dlg', 'note-dlg']) if ($(id).open) $(id).close('cancel');
    $('clip').value = '';
  }
  since = next;
  holder = state.holder;
  if (cycleChanged) keyboard.reset();
  waylandClipboard = state.wayland_clipboard === true;
  browserRunning = state.browser_running !== false;
  deadline = holder === 'human' && state.idle_remaining_ms != null ? Date.now() + state.idle_remaining_ms : 0;
  render();
}

function feedback(message, offerCopy = false) {
  clearTimeout(feedbackTimer);
  $('feedback').textContent = message;
  $('feedback').hidden = false;
  $('copy-retry').hidden = !offerCopy || !canSync() || !remoteClip;
  feedbackTimer = setTimeout(() => {
    $('feedback').hidden = true;
    $('copy-retry').hidden = true;
  }, offerCopy ? 12000 : 4600);
}

function tickIdle() {
  const left = deadline - Date.now();
  const warn = holder === 'human' && deadline !== 0 && left <= WARN_MS;
  $('idle').hidden = !warn;
  if (warn) {
    $('idle').textContent = `No input for a while: control returns to the agent in ${Math.max(0, Math.ceil(left / 1000))} s. Tap, click or type to keep it.`;
  }
}
setInterval(tickIdle, 1000);

async function post(path, body) {
  const res = await fetch(path, {
    method: 'POST', credentials: 'same-origin',
    headers: body ? { 'content-type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(await res.text());
  apply(await res.json());
}

let events;
function watchEvents() {
  events?.close();
  const source = events = new EventSource('events');
  source.addEventListener('state', e => { if (source === events) apply(JSON.parse(e.data)); });
  source.addEventListener('input', e => {
    // A modal pauses keyboard effects, but still belongs to this cycle.
    if (source !== events || !connected || holder !== 'human') return;
    let state;
    try { state = JSON.parse(e.data); } catch { /* invalidate unreadable geometry */ }
    if (state && Object.hasOwn(state, 'since') && state.since !== since) return;
    keyboard.update(state);
  });
  source.addEventListener('error', () => { if (source === events) keyboard.reset(); });
}
watchEvents();

// Keys: sticky modifiers wrap the next key, then release.
function clearSticky() {
  sticky.clear();
  document.querySelectorAll('[data-mod]').forEach(b => { b.classList.remove('on'); b.setAttribute('aria-pressed', 'false'); });
}
function tap(keysym, mods = []) {
  if (!canInput() || !keysym) return;
  const held = [...new Set([...sticky, ...mods])];
  held.forEach(m => rfb.sendKey(m, null, true));
  rfb.sendKey(keysym, null);
  held.reverse().forEach(m => rfb.sendKey(m, null, false));
  clearSticky();
}
function typeText(text) {
  for (const ch of text) tap(ch === '\n' ? KeyTable.XK_Return : keysyms.lookup(ch.codePointAt(0)));
}
$('keys').addEventListener('click', e => {
  const b = e.target.closest('button');
  if (!b || !canInput()) return;
  if (b.dataset.key) tap(KeyTable[b.dataset.key]);
  const mod = KeyTable[b.dataset.mod];
  if (mod) {
    if (sticky.has(mod)) sticky.delete(mod); else sticky.add(mod);
    b.classList.toggle('on', sticky.has(mod));
    b.setAttribute('aria-pressed', String(sticky.has(mod)));
  }
});
const trays = new Map([['tools', 'tools-btn'], ['keys', 'keys-btn'], ['clipboard', 'clip-btn']]);
let activeTray = null;
function setTray(next) {
  activeTray = next;
  for (const [id, button] of trays) {
    $(id).hidden = id !== next;
    $(button).setAttribute('aria-expanded', String(id === next));
  }
  $('tools-btn').setAttribute('aria-label', next === 'tools' ? 'Hide view controls' : 'Show view controls');
}
for (const [id, button] of trays) $(button).onclick = () => setTray(activeTray === id ? null : id);

// Soft keyboard: a hidden textarea collects typed and IME-composed text.
const NAMED = { Backspace: 'XK_BackSpace', Enter: 'XK_Return', Tab: 'XK_Tab', Escape: 'XK_Escape', Delete: 'XK_Delete',
  ArrowLeft: 'XK_Left', ArrowRight: 'XK_Right', ArrowUp: 'XK_Up', ArrowDown: 'XK_Down', Home: 'XK_Home', End: 'XK_End' };
const INPUT_KEYS = { deleteContentBackward: 'XK_BackSpace', deleteContentForward: 'XK_Delete',
  insertLineBreak: 'XK_Return', insertParagraph: 'XK_Return' };
function flushIme() { if (ime.value) typeText(ime.value); ime.value = ''; }
ime.addEventListener('keydown', e => {
  if (e.isComposing || e.keyCode === 229) return;
  if (!canInput() || handleShortcut(e, true)) return;
  const mods = [e.ctrlKey && KeyTable.XK_Control_L, e.altKey && KeyTable.XK_Alt_L, e.metaKey && KeyTable.XK_Super_L].filter(Boolean);
  if (e.shiftKey && e.key.length !== 1) mods.push(KeyTable.XK_Shift_L);
  if (NAMED[e.key]) { e.preventDefault(); tap(KeyTable[NAMED[e.key]], mods); }
  else if (mods.length && e.key.length === 1 && !(e.key.toLowerCase() === 'v' && (e.ctrlKey || e.metaKey))) { e.preventDefault(); tap(keysyms.lookup(e.key.codePointAt(0)), mods); }
});
ime.addEventListener('beforeinput', e => {
  if (canInput() && !e.isComposing && INPUT_KEYS[e.inputType]) { e.preventDefault(); tap(KeyTable[INPUT_KEYS[e.inputType]]); }
});
ime.addEventListener('input', e => { if (!e.isComposing) flushIme(); });
ime.addEventListener('compositionend', flushIme);
for (const event of ['focus', 'blur']) ime.addEventListener(event, () => {
  const active = document.activeElement === ime;
  $('kbd').setAttribute('aria-pressed', String(active));
  $('kbd').classList.toggle('on', active);
  syncViewport();
});
// Keys and toolbar taps must not steal focus from the soft keyboard.
$('hud').addEventListener('pointerdown', e => {
  const button = e.target.closest('button');
  if (button && (button === $('kbd') || document.activeElement === ime)) e.preventDefault();
});
$('kbd').onclick = keyboard.toggle; // keep focus inside the trusted activation

// Capture keyboard chords before noVNC consumes target-phase events. Leave
// native paste's default action alone: it supplies text even where readText
// is unavailable. A genuine Ctrl+V on a Mac has no native paste default.
function handleShortcut(e, soft = false) {
  const action = desktopShortcut(e, mac);
  if (!action) return false;
  if (action.copy && !e.repeat) primeCopy();
  if (!action.key && !action.paste && !action.suppress && !e.repeat) return false;
  if (!action.paste || action.read || e.repeat) e.preventDefault();
  e.stopPropagation();
  if (action.paste && action.read && !e.repeat) readLocalClipboard();
  if (action.key && (!action.copy || !e.repeat)) {
    const keysym = KeyTable[NAMED[action.key]] ?? keysyms.lookup(action.key.codePointAt(0));
    const mods = action.ctrl ? [KeyTable.XK_Control_L] : [];
    if (soft && e.shiftKey) mods.push(KeyTable.XK_Shift_L);
    tap(keysym, mods);
  }
  return true;
}
const isCanvas = target => target === $('frame').querySelector('canvas');
$('frame').addEventListener('keydown', e => {
  if (canInput() && isCanvas(e.target)) handleShortcut(e);
}, true);
for (const target of [$('frame'), ime]) target.addEventListener('paste', e => {
  if (!canInput() || (e.target !== ime && !isCanvas(e.target))) return;
  e.preventDefault();
  e.stopPropagation();
  pasteText(e.clipboardData?.getData('text/plain'));
}, true);

// Scope pending clipboard work to this connected, focused control cycle.
const clipURL = () => `clipboard?since=${encodeURIComponent(since)}`;
function updateCopy() {
  $('copy').disabled = !canSync() || !remoteClip;
  if (!remoteClip || !canSync()) $('copy-retry').hidden = true;
  $('clip-state').textContent = remoteClip ? 'Desktop text ready' : 'Desktop clipboard empty';
}
function resetClipboard() {
  generation++;
  clipSource?.close();
  clipSource = null;
  pendingCopy?.finish(null);
  remoteClip = '';
  pasteQueue = Promise.resolve();
  updateCopy();
}
function syncClipboard() {
  if (!canSync()) {
    resetClipboard();
    return;
  }
  if (!waylandClipboard || clipSource || !since) return;
  const source = new EventSource(clipURL());
  clipSource = source;
  const changed = (e, baseline) => {
    if (clipSource !== source || !canSync()) return;
    const text = JSON.parse(e.data);
    if (baseline || !text) {
      remoteClip = text ?? '';
      updateCopy();
    } else receiveClipboard(text);
  };
  source.addEventListener('baseline', e => changed(e, true));
  source.addEventListener('clipboard', e => changed(e, false));
}
window.addEventListener('focus', () => { syncClipboard(); updateCopy(); });
window.addEventListener('blur', resetClipboard);
document.addEventListener('visibilitychange', () => { syncClipboard(); updateCopy(); });

function pasteText(text) {
  if (!canSync() || !text) return;
  if (!waylandClipboard) {
    // Standalone raw-RFB viewers have no Wayland socket. Keep their original
    // VNC clipboard behavior; sessions never use this lossy legacy transport.
    rfb.clipboardPasteFrom(text);
    tap(KeyTable.XK_v, [KeyTable.XK_Control_L]);
    return;
  }
  const scope = generation, url = clipURL();
  pasteQueue = pasteQueue.then(async () => {
    if (!canSync() || generation !== scope) return;
    const res = await fetch(url, { method: 'POST', body: text });
    if (!res.ok) throw new Error('Paste was not acknowledged.');
  }).catch(() => {
    if (canSync() && generation === scope) feedback('Could not paste into the desktop. Try again.');
  });
}
function showDialog(id) {
  viewport.cancel();
  $(id).returnValue = '';
  // Do not let the browser restore focus to the hidden IME when the modal
  // closes. Preserve the cycle's cached geometry and manual override.
  keyboard.suspend();
  $(id).showModal();
  render();
}
function openClip(label, text, send) {
  $('clip-label').textContent = label;
  $('clip').value = text;
  $('clip-send').hidden = !send;
  clipDialog = { send, connection: rfb, since };
  showDialog('clip-dlg');
  $('clip').select();
}
$('clip-dlg').addEventListener('close', () => {
  const dialog = clipDialog, text = $('clip').value;
  clipDialog = null;
  $('clip').value = '';
  render();
  if (dialog?.send && dialog.connection === rfb && dialog.since === since && $('clip-dlg').returnValue === 'ok') {
    pasteText(text);
  }
});
async function readLocalClipboard() {
  if (!canSync()) return;
  const scope = generation;
  let text = '';
  try { text = await navigator.clipboard.readText(); } catch { /* permission or insecure context */ }
  if (!canSync() || generation !== scope) return;
  if (text) pasteText(text);
  else openClip('Paste the text to send to the desktop:', '', true);
}
$('paste').onclick = readLocalClipboard;

// WebKit requires starting the write inside the gesture. Supply the Blob
// only when the remote selection actually changes; reject on cancellation.
function primeCopy() {
  if (!canSync() || !window.ClipboardItem || !navigator.clipboard?.write ||
      !/AppleWebKit/.test(navigator.userAgent) || /Chrome|Chromium|Firefox/.test(navigator.userAgent)) return;
  pendingCopy?.finish(null);
  const item = Promise.withResolvers ? Promise.withResolvers() : deferred();
  const finish = text => {
    clearTimeout(timer);
    pendingCopy = null;
    if (text) item.resolve(new Blob([text], { type: 'text/plain' }));
    else item.reject(new Error('No remote copy'));
  };
  const timer = setTimeout(() => finish(null), 2000);
  // A synchronously rejected ClipboardItem must not block the remote shortcut.
  item.promise.catch(() => {});
  try {
    const write = navigator.clipboard.write([new ClipboardItem({ 'text/plain': item.promise })]);
    pendingCopy = { finish, write };
    write.catch(() => {});
  } catch { finish(null); }
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function receiveClipboard(text) {
  remoteClip = text;
  updateCopy();
  if (!text || !canSync()) return;
  const scope = generation, copy = pendingCopy;
  copy?.finish(text);
  try {
    if (copy) await copy.write;
    else await navigator.clipboard.writeText(text);
    if (canSync() && generation === scope && remoteClip === text) feedback('Copied the desktop clipboard.');
  } catch {
    if (canSync() && generation === scope && remoteClip === text) feedback('Your browser blocked clipboard sync.', true);
  }
}
async function copyLocal() {
  if (!canSync() || !remoteClip) return;
  const scope = generation, text = remoteClip;
  try {
    await navigator.clipboard.writeText(text);
    if (canSync() && scope === generation && remoteClip === text) feedback('Copied the desktop clipboard.');
  } catch {
    if (canSync() && scope === generation && remoteClip === text) openClip('Desktop clipboard (select and copy):', text, false);
  }
}
$('copy').onclick = $('copy-retry').onclick = copyLocal;

// Control.
$('take').onclick = () => post('take').then(() => { if (!viewport.coarse) rfb?.focus(); }).catch(e => feedback(e.message));
$('handback').onclick = () => { $('note').value = ''; showDialog('note-dlg'); };
$('note-dlg').addEventListener('close', async () => {
  if ($('note-dlg').returnValue !== 'ok') { render(); return; }
  releasing = true;
  render();
  try { await post('release', { note: $('note').value.trim() || null }); }
  catch (e) { feedback(e.message); }
  finally { releasing = false; render(); }
});

function modeLabel() {
  $('mode-label').textContent = viewport.trackpad ? 'Trackpad' : 'Direct Touch';
  $('mode').setAttribute('aria-pressed', String(!viewport.trackpad));
  $('gesture-hint').textContent = viewport.trackpad
    ? 'Slide to aim. Tap or double-tap to click; hold for right-click.'
    : 'Tap or double-tap where you want to click; hold for right-click.';
  $('gesture-hint').textContent += ' Pinch to zoom. Two fingers scroll; toggle Pan to move the view.';
}
$('mode').onclick = () => { viewport.toggleMode(); modeLabel(); };
$('fit').onclick = () => viewport.toggleFit();
$('pan').onclick = () => {
  const on = viewport.togglePan();
  $('pan').textContent = on ? 'Pan: on' : 'Pan: off';
  $('pan').setAttribute('aria-pressed', String(on));
};
$('scroll-up').onclick = () => viewport.scroll(8);
$('scroll-down').onclick = () => viewport.scroll(16);

const dock = $('dock'), hud = $('hud');
const visual = window.visualViewport;
function syncViewport() {
  const { style } = document.body;
  const width = visual?.width ?? window.innerWidth, height = visual?.height ?? window.innerHeight;
  style.setProperty('--view-width', width + 'px');
  style.setProperty('--view-height', height + 'px');
  style.setProperty('--view-left', (visual?.offsetLeft ?? 0) + 'px');
  style.setProperty('--view-top', (visual?.offsetTop ?? 0) + 'px');
  const typing = viewport.coarse && (document.activeElement === ime ||
    (visual?.scale === 1 && document.documentElement.clientHeight - height > 100));
  hud.dataset.edge = typing ? 'bottom' : hud.dataset.dock;
}
window.addEventListener('resize', syncViewport);
visual?.addEventListener('resize', syncViewport);
visual?.addEventListener('scroll', syncViewport);
new ResizeObserver(() => {
  document.body.style.setProperty('--hud-height', hud.offsetHeight + 'px');
}).observe(hud);
syncViewport();

let docking = null, dragged = false;
dock.addEventListener('pointerdown', e => {
  if (e.button !== 0 || docking) return;
  docking = { id: e.pointerId, x: e.clientX, y: e.clientY };
  dragged = false;
  dock.setPointerCapture(e.pointerId);
});
dock.addEventListener('pointermove', e => {
  if (docking?.id !== e.pointerId) return;
  if (!dragged && Math.hypot(e.clientX - docking.x, e.clientY - docking.y) < 12) return;
  const rect = document.body.getBoundingClientRect();
  const x = (e.clientX - rect.left) / rect.width;
  dragged = true;
  hud.dataset.dock = e.clientY - rect.top < rect.height / 2 ? 'top' : 'bottom';
  hud.dataset.align = x < .32 ? 'start' : x > .68 ? 'end' : 'center';
  syncViewport();
});
// Prevent a recognized dock drag from becoming a native touch scroll/fling.
dock.addEventListener('touchmove', e => { if (dragged) e.preventDefault(); }, { passive: false });
for (const event of ['pointerup', 'pointercancel', 'lostpointercapture']) {
  dock.addEventListener(event, e => { if (docking?.id === e.pointerId) docking = null; });
}
dock.onclick = e => {
  if (!dragged || !e.detail) hud.dataset.dock = hud.dataset.dock === 'bottom' ? 'top' : 'bottom';
  dragged = false;
  syncViewport();
};

modeLabel();
render();
connect();
