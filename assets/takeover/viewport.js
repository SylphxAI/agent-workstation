// One coordinate space for painting, mouse input and touch. noVNC paints at
// framebuffer size and owns the protocol; this surface owns the camera.
const clamp = (n, lo, hi) => Math.min(hi, Math.max(lo, n));
const dist = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
const middle = (a, b) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });

export function createViewport({ screen, frame, cursor, send, focus, enabled, changed }) {
  const coarse = matchMedia('(pointer: coarse)').matches || navigator.maxTouchPoints > 0;
  let trackpad = coarse, touchCursor = coarse, pan = false;
  let width = 0, height = 0, viewW = 0, viewH = 0;
  let scale = 1, left = 0, top = 0, preset = null;
  let pos = { x: 0, y: 0 }, mask = 0, mouseId = null, positioned = false;
  let finger = null, pinch = null, multi = false, lastTap = null, holdTimer = 0;
  let wheelX = 0, wheelY = 0, wheelPosition = null;

  const fitScale = () => Math.min(4, viewW / width, viewH / height);
  const axis = (origin, size, available) => size <= available ? (available - size) / 2 : clamp(origin, available - size, 0);
  const inFrame = (p) => p.x >= left && p.x < left + width * scale && p.y >= top && p.y < top + height * scale;
  const local = (p, rect = screen.getBoundingClientRect()) => ({ x: p.clientX - rect.left, y: p.clientY - rect.top });
  const toDesktop = (p) => ({ x: (p.x - left) / scale, y: (p.y - top) / scale });

  function paintCursor() {
    screen.dataset.trackpad = String(trackpad && touchCursor && enabled());
    cursor.hidden = !width || !trackpad || !touchCursor || !enabled();
    if (cursor.hidden) return;
    const x = left + pos.x * scale, y = top + pos.y * scale;
    const offscreen = x < 0 || x >= viewW || y < 0 || y >= viewH;
    cursor.classList.toggle('offscreen', offscreen);
    // At an on-screen location the arrow tip must mark the click point, even if the art clips.
    const tipX = offscreen ? clamp(x, 2, viewW - 22) : x;
    const tipY = offscreen ? clamp(y, 2, viewH - 26) : y;
    cursor.style.transform = 'translate(' + (tipX - 2) + 'px,' + (tipY - 2) + 'px)';
  }

  function paint() {
    if (width && height && viewW && viewH) {
      left = axis(left, width * scale, viewW);
      top = axis(top, height * scale, viewH);
      frame.style.transform = 'translate(' + left + 'px,' + top + 'px) scale(' + scale + ')';
      changed({ scale, fitted: Math.abs(scale - fitScale()) < 1e-4 });
    }
    paintCursor();
  }

  function center(which) {
    if (!width || !viewW) return;
    preset = which;
    scale = which === 'fit' ? fitScale() : Math.max(1, fitScale());
    left = (viewW - width * scale) / 2;
    top = (viewH - height * scale) / 2;
    paint();
  }

  function layout() {
    const nextW = screen.clientWidth, nextH = screen.clientHeight;
    if (!nextW || !nextH || (viewW === nextW && viewH === nextH)) return;
    const anchor = toDesktop({ x: viewW / 2, y: viewH / 2 });
    viewW = nextW;
    viewH = nextH;
    if (!width) return;
    if (preset === 'fit') { center('fit'); return; }
    scale = preset === 'focus' ? Math.max(1, fitScale()) : clamp(scale, fitScale(), 4);
    left = viewW / 2 - anchor.x * scale;
    top = viewH / 2 - anchor.y * scale;
    // Keep the last remote input point clear of the new keyboard/dock edge.
    if (coarse && positioned) {
      const margin = Math.min(32, viewW / 4, viewH / 4);
      left += clamp(left + pos.x * scale, margin, viewW - margin) - (left + pos.x * scale);
      top += clamp(top + pos.y * scale, margin, viewH - margin) - (top + pos.y * scale);
    }
    paint();
  }
  new ResizeObserver(layout).observe(screen);
  layout();

  function framebuffer(w, h) {
    if (width === w && height === h) return;
    if (!w || !h) { width = height = 0; paintCursor(); return; }
    const first = !width;
    const oldCenter = toDesktop({ x: viewW / 2, y: viewH / 2 });
    width = w;
    height = h;
    frame.style.width = w + 'px';
    frame.style.height = h + 'px';
    pos = first ? { x: w / 2, y: h / 2 } : { x: clamp(pos.x, 0, w - 1), y: clamp(pos.y, 0, h - 1) };
    if (first || preset) center(first ? (viewW < 768 ? 'focus' : 'fit') : preset);
    else {
      scale = clamp(scale, fitScale(), 4);
      left = viewW / 2 - oldCenter.x * scale;
      top = viewH / 2 - oldCenter.y * scale;
      paint();
    }
  }

  function point(p = pos, buttons = mask) {
    if (!width || !enabled()) return;
    pos = { x: clamp(p.x, 0, width - 1), y: clamp(p.y, 0, height - 1) };
    positioned = true;
    send(pos.x, pos.y, buttons);
    paintCursor();
  }
  function click(button, touch = false) {
    point(pos, mask | button);
    point();
    if (!touch) return;
    const ripple = document.createElement('span');
    ripple.className = button === 4 ? 'tap-ripple right' : 'tap-ripple';
    ripple.style.left = clamp(left + pos.x * scale, 5, viewW - 5) + 'px';
    ripple.style.top = clamp(top + pos.y * scale, 5, viewH - 5) + 'px';
    screen.append(ripple);
    setTimeout(() => ripple.remove(), 280);
  }
  function scroll(direction) {
    const anchor = !touchCursor && wheelPosition ? wheelPosition : !(trackpad && touchCursor) ? toDesktop({ x: viewW / 2, y: viewH / 2 }) : pos;
    point(anchor, mask | direction);
    point();
  }

  function stopHold() { clearTimeout(holdTimer); }
  function cancel() {
    stopHold();
    finger = pinch = lastTap = null;
    multi = false;
    mouseId = null;
    wheelX = wheelY = 0;
    if (mask) { mask = 0; point(); }
    paintCursor();
  }

  function touches(e) {
    const rect = screen.getBoundingClientRect();
    return Array.from(e.targetTouches, t => ({ id: t.identifier, ...local(t, rect) }));
  }
  function startPinch(points) {
    stopHold();
    lastTap = finger = null;
    multi = true;
    if (mask) { mask = 0; point(); }
    const [a, b] = points;
    if (!b) { pinch = null; return; }
    const mid = middle(a, b);
    pinch = { ids: [a.id, b.id], span: Math.max(1, dist(a, b)), scale,
      start: mid, anchor: toDesktop(mid), wheel: mid, kind: null };
    // Scroll the item under the trackpad pointer, or under the initial touch midpoint.
    pinch.scrollAt = trackpad ? { ...pos } : pinch.anchor;
  }

  screen.addEventListener('touchstart', e => {
    e.preventDefault();
    if (!width || mouseId !== null) return;
    touchCursor = true;
    paintCursor();
    const points = touches(e);
    if (points.length >= 2) { if (!pinch) startPinch(points); return; }
    if (multi || !enabled() || !points.length) return;
    const p = points[0], at = performance.now();
    const second = trackpad && lastTap && at - lastTap.at < 325 && dist(lastTap, p) < 34;
    lastTap = null;
    finger = { start: p, last: p, at, moved: false, held: false, second, dragging: false };
    if (!trackpad && !inFrame(p)) { finger = null; return; }
    holdTimer = setTimeout(() => {
      if (!finger || finger.moved || !enabled()) return;
      if (finger.second) { finger.dragging = true; mask = 1; point(); }
      else {
        finger.held = true;
        if (!trackpad) point(toDesktop(p));
        click(4, true);
      }
    }, second ? 220 : 510);
  }, { passive: false });

  screen.addEventListener('touchmove', e => {
    e.preventDefault();
    if (!width || mouseId !== null) return;
    const points = touches(e);
    if (points.length >= 2) {
      const pair = pinch?.ids.map(id => points.find(p => p.id === id));
      if (!pair?.every(Boolean)) { startPinch(points); return; }
      const [a, b] = pair, mid = middle(a, b);
      const span = dist(a, b);
      if (!pinch.kind) {
        const translation = dist(mid, pinch.start), spread = Math.abs(span - pinch.span);
        if (spread >= 10 && spread >= translation * .65) pinch.kind = 'camera';
        else if (translation >= 13) pinch.kind = pan || !enabled() ? 'camera' : 'scroll';
        else return;
      }
      if (pinch.kind === 'camera') {
        scale = clamp(pinch.scale * span / pinch.span, fitScale(), 4);
        left = mid.x - pinch.anchor.x * scale;
        top = mid.y - pinch.anchor.y * scale;
        preset = null;
        paint();
      } else {
        const dx = mid.x - pinch.wheel.x, dy = mid.y - pinch.wheel.y;
        // Finger motion follows page motion; RFB wheel direction is opposite.
        const horizontal = Math.abs(dx) > Math.abs(dy), delta = horizontal ? dx : dy;
        const count = Math.min(6, Math.floor(Math.abs(delta) / 18));
        if (count) {
          const direction = horizontal ? (delta > 0 ? 32 : 64) : (delta > 0 ? 8 : 16);
          for (let i = 0; i < count; i++) { point(pinch.scrollAt, mask | direction); point(); }
          pinch.wheel = mid;
        }
      }
    } else if (!multi && finger && enabled() && points[0]?.id === finger.start.id) {
      const p = points[0], at = performance.now();
      if (!finger.moved && dist(p, finger.start) < 5) return;
      stopHold();
      finger.moved = true;
      if (!trackpad) return;
      if (finger.second && !finger.dragging) { finger.dragging = true; mask = 1; point(); }
      const speed = dist(finger.last, p) / clamp(at - finger.at, 5, 60);
      const gain = clamp(.62 + speed * .95, .62, 3.8) / Math.min(1.4, Math.max(1, scale));
      point({ x: pos.x + (p.x - finger.last.x) * gain, y: pos.y + (p.y - finger.last.y) * gain });
      finger.last = p;
      finger.at = at;
    }
  }, { passive: false });

  screen.addEventListener('touchend', e => {
    e.preventDefault();
    stopHold();
    if (multi) {
      const points = touches(e);
      if (points.length >= 2 && !pinch?.ids.every(id => points.some(p => p.id === id))) startPinch(points);
      if (!points.length) { multi = false; pinch = null; }
      return;
    }
    if (!finger) return;
    const end = Array.from(e.changedTouches).find(t => t.identifier === finger.start.id);
    if (!end) return;
    if (finger.dragging) { mask = 0; point(); }
    else if (!finger.moved && !finger.held && enabled()) {
      const p = local(end);
      if (trackpad) { click(1, true); lastTap = { ...p, at: performance.now() }; }
      else if (inFrame(p)) { point(toDesktop(p)); click(1, true); }
    }
    finger = null;
  }, { passive: false });
  screen.addEventListener('touchcancel', cancel);
  screen.addEventListener('contextmenu', e => e.preventDefault());

  // Pointer Events handle only mouse/pen; simultaneous touch samples above
  // avoid mapping a halfway-updated midpoint or firing synthetic mouse clicks.
  const buttonMask = (buttons) => (buttons & 1) | ((buttons & 2) << 1) | ((buttons & 4) >> 1) | ((buttons & 24) << 4);
  screen.addEventListener('pointerdown', e => {
    if (e.pointerType === 'touch' || finger || multi || !enabled()) return;
    const p = local(e);
    if (!inFrame(p) && mouseId === null) return;
    e.preventDefault();
    touchCursor = false;
    if (mouseId === null) {
      mouseId = e.pointerId;
      screen.setPointerCapture(e.pointerId);
      focus();
    }
    mask = buttonMask(e.buttons);
    point(toDesktop(p));
  });
  screen.addEventListener('pointermove', e => {
    if (e.pointerType === 'touch' || finger || multi || !enabled()) return;
    const p = local(e);
    if (inFrame(p) || mouseId === e.pointerId) {
      touchCursor = false;
      mask = mouseId === e.pointerId ? buttonMask(e.buttons) : 0;
      point(toDesktop(p));
    }
  });
  screen.addEventListener('pointerup', e => {
    if (mouseId !== e.pointerId) return;
    mask = buttonMask(e.buttons);
    point(toDesktop(local(e)));
    mouseId = null;
  });
  screen.addEventListener('lostpointercapture', e => { if (e.pointerId === mouseId) cancel(); });
  screen.addEventListener('pointercancel', e => { if (e.pointerId === mouseId) cancel(); });
  screen.addEventListener('wheel', e => {
    const p = local(e);
    if (!enabled() || !inFrame(p)) return;
    e.preventDefault();
    touchCursor = false;
    wheelPosition = toDesktop(p);
    const step = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 : 1;
    wheelX = clamp(wheelX + e.deltaX * step, -240, 240);
    wheelY = clamp(wheelY + e.deltaY * step, -240, 240);
    while (Math.abs(wheelX) >= 40) { scroll(wheelX < 0 ? 32 : 64); wheelX -= 40 * Math.sign(wheelX); }
    while (Math.abs(wheelY) >= 40) { scroll(wheelY < 0 ? 8 : 16); wheelY -= 40 * Math.sign(wheelY); }
    wheelPosition = null;
  }, { passive: false });
  window.addEventListener('blur', cancel);

  return { framebuffer, center, cancel, scroll, coarse, paintCursor,
    get trackpad() { return trackpad; },
    togglePan() { cancel(); pan = !pan; return pan; },
    toggleMode() { cancel(); trackpad = !trackpad; touchCursor = trackpad; paintCursor(); },
    toggleFit() { cancel(); center(Math.abs(scale - fitScale()) < 1e-4 ? 'focus' : 'fit'); },
  };
}
