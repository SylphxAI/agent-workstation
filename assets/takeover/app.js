// Takeover page: the desktop over noVNC, gated by who holds control.
import RFB from './novnc/core/rfb.js';
import KeyTable from './novnc/core/input/keysym.js';
import keysyms from './novnc/core/input/keysymdef.js';

const $ = id => document.getElementById(id);
// The token now lives in an HttpOnly cookie; keep it out of history and screenshots.
history.replaceState(null, '', location.pathname);

const WARN_MS = 30_000;
let rfb = null, holder = 'agent', deadline = 0, remoteClip = '', fit = true, sendClip = false;
const sticky = new Set();

function connect() {
  const url = new URL('ws', location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  rfb = new RFB($('screen'), url.href, { wsProtocols: ['binary'] });
  rfb.scaleViewport = fit;
  rfb.clipViewport = !fit;
  rfb.showDotCursor = true;
  rfb.viewOnly = holder !== 'human';
  rfb.addEventListener('connect', render);
  rfb.addEventListener('disconnect', () => { rfb = null; render(); setTimeout(connect, 2000); });
  rfb.addEventListener('clipboard', e => { remoteClip = e.detail.text; render(); });
}

function render() {
  const human = holder === 'human';
  $('status').textContent = !rfb ? 'Disconnected, reconnecting…'
    : human ? 'You are in control. The agent is paused.' : 'The agent is in control (view only).';
  $('take').hidden = human;
  $('handback').hidden = !human;
  document.querySelectorAll('.needs-control, #keys button').forEach(b => { b.disabled = !human; });
  $('copy').disabled = !remoteClip;
  $('fit').textContent = fit ? '1:1' : 'Fit';
  $('pan').hidden = fit;
  $('pan').classList.toggle('on', !!rfb?.dragViewport);
  $('screen').classList.toggle('view-only', !human);
  if (!human) { $('keys').hidden = true; $('ime').blur(); clearSticky(); }
  if (rfb) rfb.viewOnly = !human;
  tickIdle();
}

function apply(state) {
  holder = state.holder;
  deadline = holder === 'human' ? Date.now() + state.idle_remaining_ms : 0;
  render();
}

function tickIdle() {
  const left = deadline - Date.now();
  const warn = holder === 'human' && left <= WARN_MS;
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
  document.querySelectorAll('[data-mod]').forEach(b => b.classList.remove('on'));
}
function tap(keysym, mods = []) {
  if (!rfb || !keysym) return;
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
  if (mod) b.classList.toggle('on', sticky.has(mod) ? !sticky.delete(mod) : !!sticky.add(mod));
});
$('keys-btn').onclick = () => { $('keys').hidden = !$('keys').hidden; };

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
  if (NAMED[e.key]) { e.preventDefault(); tap(KeyTable[NAMED[e.key]], mods); }
  else if (mods.length && e.key.length === 1) { e.preventDefault(); tap(keysyms.lookup(e.key.codePointAt(0)), mods); }
});
ime.addEventListener('beforeinput', e => {
  if (!e.isComposing && INPUT_KEYS[e.inputType]) { e.preventDefault(); tap(KeyTable[INPUT_KEYS[e.inputType]]); }
});
ime.addEventListener('input', e => { if (!e.isComposing) flushIme(); });
ime.addEventListener('compositionend', flushIme);
$('kbd').onclick = () => (document.activeElement === ime ? ime.blur() : ime.focus());

// Clipboard: local text becomes the remote clipboard, then Ctrl+V pastes it.
function pasteText(text) {
  if (!rfb || !text) return;
  rfb.clipboardPasteFrom(text);
  tap(KeyTable.XK_v, [KeyTable.XK_Control_L]);
}
function openClip(label, text, send) {
  sendClip = send;
  $('clip-label').textContent = label;
  $('clip').value = text;
  $('clip-send').hidden = !send;
  $('clip-dlg').showModal();
  $('clip').select();
}
$('clip-dlg').addEventListener('close', () => {
  if (sendClip && $('clip-dlg').returnValue === 'ok') pasteText($('clip').value);
});
$('paste').onclick = async () => {
  let text = '';
  try { text = await navigator.clipboard.readText(); } catch { /* permission or insecure context */ }
  if (text) pasteText(text);
  else openClip('Paste the text to send to the desktop:', '', true);
};
$('copy').onclick = async () => {
  try { await navigator.clipboard.writeText(remoteClip); $('status').textContent = 'Copied the desktop clipboard.'; }
  catch { openClip('Desktop clipboard (select and copy):', remoteClip, false); }
};

// Control.
$('take').onclick = () => post('take').then(() => rfb?.focus()).catch(e => { $('status').textContent = e.message; });
$('handback').onclick = () => { $('note').value = ''; $('note-dlg').showModal(); };
$('note-dlg').addEventListener('close', () => {
  if ($('note-dlg').returnValue !== 'ok') return;
  clearSticky();
  post('release', { note: $('note').value.trim() || null }).catch(e => { $('status').textContent = e.message; });
});

// View: fit the whole desktop, or actual size with optional drag-to-pan.
$('fit').onclick = () => {
  fit = !fit;
  if (rfb) { rfb.dragViewport = false; rfb.scaleViewport = fit; rfb.clipViewport = !fit; }
  render();
};
$('pan').onclick = () => { if (rfb) rfb.dragViewport = !rfb.dragViewport; render(); };

render();
connect();
