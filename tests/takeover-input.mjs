import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';

// The browser uses .js ES modules without needing a package.json or Node deps.
const source = await readFile(new URL('../assets/takeover/input.js', import.meta.url));
const { desktopShortcut } = await import(`data:text/javascript;base64,${source.toString('base64')}`);
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
