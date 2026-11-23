import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';

// The browser uses .js ES modules without needing a package.json or Node deps.
async function module(name) {
  const source = await readFile(new URL(`../assets/takeover/${name}.js`, import.meta.url));
  return import(`data:text/javascript;base64,${source.toString('base64')}`);
}
const { createSoftKeyboard, desktopShortcut, requiresKeyboardGesture } = await module('input');
const { createViewport } = await module('viewport');
const mac = (key, mods = {}) => desktopShortcut({ key, metaKey: true, ...mods }, true);
const ctrl = (key, options = {}, isMac = false) => desktopShortcut({ key, ctrlKey: true, ...options }, isMac);

test('macOS editing and browser commands become remote Control commands', () => {
  for (const key of ['a', 'z', 'y', 'f', 'l', 'r', 'k']) {
    assert.deepEqual(mac(key), { key, ctrl: true, copy: false });
  }
  assert.deepEqual(mac('Z', { shiftKey: true }), { key: 'z', ctrl: true, copy: false });
  for (const key of ['c', 'x']) assert.deepEqual(mac(key), { key, ctrl: true, copy: true });
  assert.deepEqual(mac('v'), { paste: true, read: false });
  assert.deepEqual(mac('Meta'), { suppress: true });
});

test('macOS navigation preserves the remote app’s selection conventions', () => {
  assert.deepEqual(mac('ArrowLeft', { shiftKey: true }), { key: 'Home' });
  assert.deepEqual(mac('ArrowRight'), { key: 'End' });
  assert.deepEqual(mac('Backspace'), { key: 'Backspace', ctrl: true });
  for (const key of ['ArrowLeft', 'ArrowRight']) {
    assert.deepEqual(mac(key, { metaKey: false, altKey: true }), { key, ctrl: true });
  }
});

test('native Control chords, typing and IME retain their usual paths', () => {
  assert.deepEqual(ctrl('v'), { paste: true, read: false });
  assert.deepEqual(ctrl('v', {}, true), { paste: true, read: true });
  for (const key of ['c', 'x']) assert.deepEqual(ctrl(key), { copy: true });
  for (const key of ['a', 'z', 'Backspace', 'ArrowLeft']) assert.equal(ctrl(key), null);
  assert.equal(ctrl('v', { altKey: true, getModifierState: name => name === 'AltGraph' }), null);
  assert.equal(mac('c', { isComposing: true }), null);
  assert.equal(mac('v', { keyCode: 229 }), null);
  assert.equal(desktopShortcut({ key: '粵' }, true), null);
  assert.equal(desktopShortcut({ key: 'a', metaKey: true }, false), null);
});

const input = (active = false, purpose = 'text', rects = [[350, 260, 20, 20]]) => ({ active, purpose, rects });
const hit = { x: 351, y: 270 }, miss = { x: 370, y: 270 }; // exclusive right edge
function keyboardFixture({ coarse = true, asyncFocus = true } = {}) {
  const trace = [];
  const fixture = { focused: false, allowed: true };
  const keyboard = createSoftKeyboard({
    coarse: () => coarse,
    canInput: () => fixture.allowed,
    asyncFocus: () => asyncFocus,
    focused: () => fixture.focused,
    focus: purpose => { trace.push(['focus', purpose]); fixture.focused = true; },
    blur: () => { trace.push(['blur']); fixture.focused = false; },
  });
  return Object.assign(fixture, { keyboard, trace });
}

test('touch taps use half-open cached bounds; misses never focus before dismissing', () => {
  const { keyboard, trace } = keyboardFixture();
  keyboard.tap(hit); // no metadata, e.g. raw RFB
  keyboard.update(input(false, 'email', [[350, 260, 20, 20], [370, 260, 0, 20], ['370', 260, 20, 20]]));
  keyboard.tap(miss);
  assert.deepEqual(trace, []);
  keyboard.tap(hit);
  keyboard.tap(hit); // no blur/refocus or keyboard-layout churn
  assert.deepEqual(trace, [['focus', 'email']]);
  keyboard.tap(miss);
  assert.deepEqual(trace, [['focus', 'email'], ['blur']]);
});

test('active SSE edges show and dismiss, but duplicate or geometry-only updates never reopen', () => {
  const { keyboard, trace } = keyboardFixture();
  keyboard.update(input(true, 'url'));
  keyboard.tap(miss);
  keyboard.update(input(true, 'search', [[10, 10, 30, 30]]));
  assert.deepEqual(trace, [['focus', 'url'], ['blur']]);
  keyboard.update(input());
  keyboard.update(input(true, 'decimal'));
  keyboard.update(input(true));
  keyboard.update(input());
  keyboard.update(input());
  assert.deepEqual(trace, [['focus', 'url'], ['blur'], ['focus', 'decimal'], ['blur']]);
});

test('manual pin survives misses and SSE blur until a hit; suppress survives SSE focus until a hit', () => {
  const { keyboard, trace } = keyboardFixture();
  keyboard.update(input(true));
  keyboard.toggle(); // suppress
  keyboard.update(input());
  keyboard.update(input(true));
  keyboard.tap(miss);
  assert.deepEqual(trace, [['focus', 'text'], ['blur']]);
  keyboard.tap(hit); // clears suppress synchronously
  keyboard.update(input());
  assert.deepEqual(trace, [['focus', 'text'], ['blur'], ['focus', 'text'], ['blur']]);
  trace.length = 0;
  keyboard.toggle(); // pin
  keyboard.update(input(true));
  keyboard.update(input());
  keyboard.tap(miss);
  assert.deepEqual(trace, [['focus', 'text']]);
  keyboard.tap(hit); // clears pin without replacing the focused textarea
  keyboard.tap(miss);
  assert.deepEqual(trace, [['focus', 'text'], ['blur']]);
});

test('iOS and iPadOS never take async focus but the first hit focuses and inactive SSE blurs', () => {
  const devices = [
    { userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_4) AppleWebKit/605.1.15 CriOS/135', platform: 'iPhone', maxTouchPoints: 5 },
    { userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15', platform: 'MacIntel', maxTouchPoints: 5 },
  ];
  for (const device of devices) {
    assert.equal(requiresKeyboardGesture(device), true);
    const { keyboard, trace } = keyboardFixture({ asyncFocus: !requiresKeyboardGesture(device) });
    keyboard.update(input(true, 'search'));
    assert.deepEqual(trace, [], 'no hidden async focus before the gesture');
    keyboard.tap(hit);
    assert.deepEqual(trace, [['focus', 'search']]);
    keyboard.update(input());
    assert.deepEqual(trace, [['focus', 'search'], ['blur']]);
  }
  assert.equal(requiresKeyboardGesture({ userAgent: devices[1].userAgent, platform: 'MacIntel', maxTouchPoints: 0 }), false);
  assert.equal(requiresKeyboardGesture({ userAgent: 'Mozilla/5.0 (Linux; Android 16)', platform: 'Linux armv8l', maxTouchPoints: 5 }), false);
});

test('desktop never auto-focuses or auto-blurs; manual keyboard still works', () => {
  const { keyboard, trace } = keyboardFixture({ coarse: false });
  keyboard.update(input(true));
  keyboard.tap(hit);
  assert.deepEqual(trace, []);
  keyboard.toggle();
  keyboard.tap(miss);
  keyboard.update(input());
  assert.deepEqual(trace, [['focus', 'text']]);
  keyboard.reset();
  assert.deepEqual(trace, [['focus', 'text'], ['blur']]);
});

test('modal pause preserves geometry and suppression; custody reset forgets both', () => {
  const fixture = keyboardFixture();
  const { keyboard, trace } = fixture;
  keyboard.update(input(true));
  keyboard.toggle(); // suppress
  fixture.allowed = false;
  keyboard.suspend();
  keyboard.update(input(false, 'email'));
  keyboard.tap(hit);
  keyboard.toggle();
  fixture.allowed = true;
  keyboard.update(input(true, 'email'));
  assert.deepEqual(trace, [['focus', 'text'], ['blur']], 'suppression survives modal');
  keyboard.tap(hit);
  assert.deepEqual(trace.at(-1), ['focus', 'email'], 'cached bounds survive modal');
  keyboard.toggle(); // suppress again
  keyboard.reset();
  trace.length = 0;
  keyboard.tap(hit);
  assert.deepEqual(trace, [], 'old bounds cannot focus in the new cycle');
  keyboard.update(input(true, 'unrecognized'));
  assert.deepEqual(trace, [['focus', 'text']], 'old suppression cannot block a new cycle');
  keyboard.update(null);
  assert.deepEqual(trace, [['focus', 'text'], ['blur']], 'unreadable snapshot cannot retain stale active bounds');
});

function viewportFixture(t) {
  const port = () => Object.assign(new EventTarget(), { style: {}, dataset: {}, classList: { toggle() {} } });
  const screen = Object.assign(port(), {
    clientWidth: 400, clientHeight: 350, append() {}, setPointerCapture() {},
    getBoundingClientRect: () => ({ left: 0, top: 0 }),
  });
  const globals = {
    matchMedia: () => ({ matches: true }), window: port(),
    ResizeObserver: class { observe() {} },
    document: { createElement: () => ({ style: {}, remove() {} }) },
  };
  for (const [key, value] of Object.entries(globals)) {
    const original = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, value });
    t.after(() => original ? Object.defineProperty(globalThis, key, original) : delete globalThis[key]);
  }
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const fixture = keyboardFixture({ asyncFocus: false });
  const trace = fixture.trace;
  let inEvent = false;
  const viewport = createViewport({
    screen, frame: port(), cursor: port(), enabled: () => fixture.allowed,
    focus: () => trace.push(['canvas focus']), changed() {},
    send: (x, y, mask) => trace.push(['rfb', Math.round(x), Math.round(y), mask]),
    onTap: touch => {
      assert.equal(inEvent, true, 'touch callback must not yield to the event loop');
      trace.push(['tap', touch]);
      fixture.keyboard.tap(touch);
    },
  });
  viewport.framebuffer(800, 600);
  const finger = (clientX, clientY, identifier = 1) => ({ clientX, clientY, identifier });
  function dispatch(type, props) {
    const event = Object.assign(new Event(type, { cancelable: true }), props);
    inEvent = true;
    try { screen.dispatchEvent(event); } finally { inEvent = false; }
    if (type !== 'touchcancel') assert.equal(event.defaultPrevented, true);
  }
  const touch = (type, targetTouches, changedTouches = targetTouches) => dispatch(type, { targetTouches, changedTouches });
  const start = p => touch('touchstart', [p]);
  const end = p => touch('touchend', [], [p]);
  return Object.assign(fixture, { viewport, finger, dispatch, touch, start, end });
}

test('Direct Touch and Trackpad commit before synchronous focus; repeat taps do not flicker', t => {
  const { viewport, trace, keyboard, finger, start, end } = viewportFixture(t);
  keyboard.update(input(false, 'email', [[395, 295, 15, 15]]));
  const p = finger(150, 100); // not itself over the centered remote hitbox
  start(p); end(p);
  assert.deepEqual(trace, [
    ['rfb', 400, 300, 1], ['rfb', 400, 300, 0], ['tap', { x: 400, y: 300, trackpad: true }], ['focus', 'email'],
  ]);
  viewport.toggleMode();
  viewport.toggleFit(); // fit to scale .5 and y offset 25
  keyboard.update(input(false, 'text'));
  trace.length = 0;
  const q = finger(175.4, 160);
  start(q); end(q);
  assert.deepEqual(trace, [
    ['rfb', 351, 270, 0], ['rfb', 351, 270, 1], ['rfb', 351, 270, 0], ['tap', { x: 351, y: 270, trackpad: false }],
  ]);
  const outside = finger(185, 160);
  start(outside); end(outside);
  assert.deepEqual(trace.slice(-3), [['rfb', 370, 270, 0], ['tap', { x: 370, y: 270, trackpad: false }], ['blur']]);
  trace.length = 0;
  start(q); end(q);
  assert.deepEqual(trace.at(-1), ['focus', 'text']);
});

test('hold, drag, pinch, cancellation, padding, mouse, and disabled input never emit a primary tap', t => {
  const fixture = viewportFixture(t);
  const { viewport, trace, finger, dispatch, touch, start, end, keyboard } = fixture;
  keyboard.update(input(false, 'text', [[0, 0, 800, 600]]));
  const p = finger(150, 100), moved = finger(175, 110), other = finger(160, 150, 2);
  start(p);
  t.mock.timers.tick(510);
  end(p);
  assert.deepEqual(trace, [['rfb', 400, 300, 4], ['rfb', 400, 300, 0]], 'long press is a right click only');
  trace.length = 0;
  start(p); end(p);
  assert.deepEqual(trace.at(-1), ['focus', 'text']);
  trace.length = 0;
  start(p);
  touch('touchmove', [moved]);
  end(moved);
  assert.deepEqual(trace.map(x => x[3]), [1, 1, 0], 'double-tap drag still presses, moves and releases');
  trace.length = 0;
  start(p); touch('touchstart', [p, other]);
  touch('touchmove', [p, finger(175, 180, 2)]);
  touch('touchend', [p], [other]); end(p);
  start(p); touch('touchcancel', [], [p]); // cancel has no default to prevent
  viewport.toggleMode(); viewport.toggleFit();
  start(finger(150, 8)); end(finger(150, 8)); // letterbox
  start(p); end(moved); // movement without a final touchmove is not a tap
  dispatch('pointerdown', { pointerType: 'mouse', pointerId: 1, buttons: 1, ...p });
  assert.deepEqual(trace, [['canvas focus'], ['rfb', 300, 150, 1]]);
  viewport.cancel(); trace.length = 0;
  start(p);
  // A gesture that began with control but ends without it is refused.
  fixture.allowed = false;
  end(p);
  assert.deepEqual(trace, []);
});
