// Takeover page: the desktop over noVNC, gated by who holds control.
import { createViewport } from './viewport.js';
import RFB from './novnc/core/rfb.js';
import KeyTable from './novnc/core/input/keysym.js';
import keysyms from './novnc/core/input/keysymdef.js';

const $ = id => document.getElementById(id);
// The token now lives in an HttpOnly cookie; keep it out of history and screenshots.
history.replaceState(null, '', location.pathname);

const WARN_MS = 30_000;
let browserRunning = true;
let rfb = null, connected = false, holder = 'agent', deadline = 0, remoteClip = '', sendClip = false;
let feedbackTimer;
const canInput = () => connected && holder === 'human';
const sticky = new Set();

const viewport = createViewport({
  screen: $('screen'), frame: $('frame'), cursor: $('cursor'), enabled: canInput,
  send: (x, y, mask) => rfb?.sendPointer(x, y, mask),
  focus: () => { if (document.activeElement !== $('ime')) rfb?.focus({ preventScroll: true }); },
  changed: ({ scale, fitted }) => {
    $('zoom').textContent = Math.round(scale * 100) + '%';
    $('fit').textContent = fitted ? 'Focus 100%' : 'Fit';
  },
});

function connect() {
  const url = new URL('ws', location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  const connection = new RFB($('frame'), url.href, { wsProtocols: ['binary'] });
  rfb = connection;
  connection.viewOnly = holder !== 'human';
  let canvas;
  // ResizeObserver measures untransformed CSS size. With noVNC scaling and
  // clipping disabled that size is exactly the native framebuffer size.
  const observer = new ResizeObserver(() => {
    if (connected && rfb === connection) viewport.framebuffer(canvas.width, canvas.height);
  });
  connection.addEventListener('connect', () => {
    canvas = $('frame').querySelector('canvas');
    observer.observe(canvas);
    connected = true;
    viewport.framebuffer(canvas.width, canvas.height);
    render();
  });
  connection.addEventListener('disconnect', () => {
    observer.disconnect();
    connected = false;
    viewport.cancel();
    rfb = null;
    remoteClip = '';
    render();
    setTimeout(connect, 2000);
  });
  connection.addEventListener('clipboard', e => { remoteClip = e.detail.text; render(); });
}

function render() {
  const human = holder === 'human';
  $('browser-exited').hidden = browserRunning;
  $('browser-exited-human').hidden = !human;
  $('browser-exited-agent').hidden = human;
  const status = !browserRunning ? 'The browser has stopped.' : !connected ? 'Desktop disconnected, reconnecting…'
    : human ? 'You are in control. The agent is paused.' : 'The agent is in control (view only).';
  if ($('status').textContent !== status) $('status').textContent = status;
  $('signal').dataset.state = !browserRunning || !connected ? 'waiting' : human ? 'control' : 'view';
  $('tools-btn').title = status;
  $('take').hidden = human;
  $('take').disabled = !browserRunning;
  $('handback').hidden = !human;
  document.querySelectorAll('.needs-control, #keys button').forEach(b => { b.disabled = !canInput(); });
  $('copy').disabled = !remoteClip;
  if (!canInput()) { setKeys(false); $('ime').blur(); clearSticky(); }
  if (rfb) rfb.viewOnly = !human;
  viewport.paintCursor();
  tickIdle();
}

function apply(state) {
  if (holder === 'human' && state.holder !== 'human') viewport.cancel();
  holder = state.holder;
  browserRunning = state.browser_running !== false;
  deadline = holder === 'human' && state.idle_remaining_ms != null ? Date.now() + state.idle_remaining_ms : 0;
  render();
}

function feedback(message) {
  clearTimeout(feedbackTimer);
  $('feedback').textContent = message;
  $('feedback').hidden = false;
  feedbackTimer = setTimeout(() => { $('feedback').hidden = true; }, 4600);
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

const events = new EventSource('events');
events.addEventListener('state', e => apply(JSON.parse(e.data)));

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
  if (!b) return;
  if (b.dataset.key) tap(KeyTable[b.dataset.key]);
  const mod = KeyTable[b.dataset.mod];
  if (mod) {
    if (sticky.has(mod)) sticky.delete(mod); else sticky.add(mod);
    b.classList.toggle('on', sticky.has(mod));
    b.setAttribute('aria-pressed', String(sticky.has(mod)));
  }
});
function setKeys(open) {
  $('keys').hidden = !open;
  $('keys-btn').setAttribute('aria-expanded', String(open));
}
$('keys-btn').onclick = () => setKeys($('keys').hidden);

// Soft keyboard: a hidden textarea collects typed and IME-composed text.
const ime = $('ime');
const NAMED = { Backspace: 'XK_BackSpace', Enter: 'XK_Return', Tab: 'XK_Tab', Escape: 'XK_Escape', Delete: 'XK_Delete',
  ArrowLeft: 'XK_Left', ArrowRight: 'XK_Right', ArrowUp: 'XK_Up', ArrowDown: 'XK_Down', Home: 'XK_Home', End: 'XK_End' };
const INPUT_KEYS = { deleteContentBackward: 'XK_BackSpace', deleteContentForward: 'XK_Delete',
  insertLineBreak: 'XK_Return', insertParagraph: 'XK_Return' };
function flushIme() { if (ime.value) typeText(ime.value); ime.value = ''; }
ime.addEventListener('keydown', e => {
  if (e.isComposing || e.keyCode === 229) return;
  const mods = [e.ctrlKey && KeyTable.XK_Control_L, e.altKey && KeyTable.XK_Alt_L, e.metaKey && KeyTable.XK_Super_L].filter(Boolean);
  if (e.shiftKey && e.key.length !== 1) mods.push(KeyTable.XK_Shift_L);
  if (NAMED[e.key]) { e.preventDefault(); tap(KeyTable[NAMED[e.key]], mods); }
  else if (mods.length && e.key.length === 1 && !(e.key.toLowerCase() === 'v' && (e.ctrlKey || e.metaKey))) { e.preventDefault(); tap(keysyms.lookup(e.key.codePointAt(0)), mods); }
});
ime.addEventListener('beforeinput', e => {
  if (!e.isComposing && INPUT_KEYS[e.inputType]) { e.preventDefault(); tap(KeyTable[INPUT_KEYS[e.inputType]]); }
});
ime.addEventListener('input', e => { if (!e.isComposing) flushIme(); });
ime.addEventListener('compositionend', flushIme);
ime.addEventListener('paste', e => {
  const text = e.clipboardData?.getData('text/plain');
  if (text) { e.preventDefault(); pasteText(text); }
});
for (const event of ['focus', 'blur']) ime.addEventListener(event, () => {
  const active = document.activeElement === ime;
  $('kbd').setAttribute('aria-pressed', String(active));
  $('kbd').classList.toggle('on', active);
});
$('kbd').addEventListener('pointerdown', e => e.preventDefault());
$('kbd').onclick = () => {
  if (document.activeElement === ime) ime.blur();
  else ime.focus({ preventScroll: true }); // must run inside the iOS tap activation
};

// Clipboard: local text becomes the remote clipboard, then Ctrl+V pastes it.
function pasteText(text) {
  if (!canInput() || !text) return;
  rfb.clipboardPasteFrom(text);
  tap(KeyTable.XK_v, [KeyTable.XK_Control_L]);
}
function openClip(label, text, send) {
  sendClip = send;
  $('clip-label').textContent = label;
  $('clip').value = text;
  $('clip-send').hidden = !send;
  $('clip-dlg').returnValue = '';
  $('clip-dlg').showModal();
  $('clip').select();
}
$('clip-dlg').addEventListener('close', () => {
  if (sendClip && $('clip-dlg').returnValue === 'ok') pasteText($('clip').value);
});
$('paste').onclick = async () => {
  let text = '';
  try { text = await navigator.clipboard.readText(); } catch { /* permission or insecure context */ }
  if (!canInput()) return;
  if (text) pasteText(text);
  else openClip('Paste the text to send to the desktop:', '', true);
};
$('copy').onclick = async () => {
  try { await navigator.clipboard.writeText(remoteClip); feedback('Copied the desktop clipboard.'); }
  catch { openClip('Desktop clipboard (select and copy):', remoteClip, false); }
};

// Control.
$('take').onclick = () => post('take').then(() => { if (!viewport.coarse) rfb?.focus(); }).catch(e => feedback(e.message));
$('handback').onclick = () => { $('note').value = ''; $('note-dlg').returnValue = ''; $('note-dlg').showModal(); };
$('note-dlg').addEventListener('close', () => {
  if ($('note-dlg').returnValue !== 'ok') return;
  clearSticky();
  viewport.cancel();
  post('release', { note: $('note').value.trim() || null }).catch(e => feedback(e.message));
});

function modeLabel() {
  $('mode-label').textContent = viewport.trackpad ? 'Trackpad' : 'Direct Touch';
  $('mode').setAttribute('aria-pressed', String(!viewport.trackpad));
  $('gesture-hint').textContent = viewport.trackpad
    ? 'Slide one finger to aim, tap to click, or hold for right-click. Pinch or pan with two fingers.'
    : 'Tap to click where you touch. Pinch or pan with two fingers.';
}
$('mode').onclick = () => { viewport.toggleMode(); modeLabel(); };
$('fit').onclick = () => viewport.toggleFit();
$('scroll-up').onclick = () => viewport.scroll(8);
$('scroll-down').onclick = () => viewport.scroll(16);

function setTools(open) {
  $('tools').hidden = !open;
  $('tools-btn').setAttribute('aria-expanded', String(open));
  $('tools-btn').setAttribute('aria-label', open ? 'Hide controls' : 'Show controls');
}
$('tools-btn').onclick = () => setTools($('tools').hidden);
const dock = $('dock'), hud = $('hud');
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
  const rect = $('screen').getBoundingClientRect();
  const x = (e.clientX - rect.left) / rect.width;
  dragged = true;
  hud.dataset.dock = e.clientY - rect.top < rect.height / 2 ? 'top' : 'bottom';
  hud.dataset.align = x < .32 ? 'start' : x > .68 ? 'end' : 'center';
});
for (const event of ['pointerup', 'pointercancel', 'lostpointercapture']) {
  dock.addEventListener(event, e => { if (docking?.id === e.pointerId) docking = null; });
}
dock.onclick = e => {
  if (!dragged || !e.detail) hud.dataset.dock = hud.dataset.dock === 'bottom' ? 'top' : 'bottom';
  dragged = false;
};

modeLabel();
render();
connect();
