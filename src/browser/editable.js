// Evaluated in a CDP isolated world, without universal cross-origin access.
// The only result fields are focus, a fixed keyboard hint, and desktop pixels.
((windowRect) => {
    if (document.visibilityState !== 'visible') return null;
    const [wx, wy, ww, wh] = windowRect;
    const dpr = devicePixelRatio;
    const vv = visualViewport;
    const chromeHeight = wh - innerHeight * dpr;
    // On the scale-1 Wayland session the page fills the window horizontally
    // and is bottom-aligned under the browser toolbar. Do not guess an origin
    // for emulated/mobile viewports, docked side panes, or changed window size.
    if (!vv || !(dpr > 0) || !(vv.scale > 0) ||
        Math.abs(ww - innerWidth * dpr) > 2 || chromeHeight < -2 || chromeHeight > 180) return null;
    const scale = dpr * vv.scale;
    const viewportClip = [0, 0, Math.min(ww, vv.width * scale), Math.min(wh - chromeHeight, vv.height * scale)];
    const top = { doc: document, parent: null, owner: null, clip: viewportClip,
        x: -vv.offsetLeft * scale, y: -vv.offsetTop * scale, sx: scale, sy: scale };
    const deadline = performance.now() + 24;
    const rects = new Map();
    const nativeInputs = ['text', 'password', 'email', 'url', 'search', 'tel', 'number'];
    const purposes = new Map([['email', 'email'], ['url', 'url'], ['search', 'search'],
        ['number', 'decimal'], ['decimal', 'decimal'], ['numeric', 'decimal'], ['tel', 'decimal']]);

    function composedParent(e) {
        return e.assignedSlot || e.parentElement || e.getRootNode().host || null;
    }

    function purpose(e) {
        if (!e || e.namespaceURI !== 'http://www.w3.org/1999/xhtml') return null;
        const type = e.localName === 'input' ? e.type : '';
        if (type || e.localName === 'textarea') {
            if ((type && !nativeInputs.includes(type)) || e.readOnly || e.matches(':disabled')) return null;
        } else if (!e.hasAttribute('contenteditable') || !e.isContentEditable) {
            return null;
        }
        const mode = e.inputMode;
        if (mode === 'none') return null;
        return purposes.get(mode) || (mode === 'text' ? 'text' : purposes.get(type)) || 'text';
    }

    const fromRect = r => [r.left, r.top, r.right, r.bottom];
    const intersect = (a, b) => [Math.max(a[0], b[0]), Math.max(a[1], b[1]),
        Math.min(a[2], b[2]), Math.min(a[3], b[3])];
    const valid = r => r.every(Number.isFinite) && r[2] > r[0] && r[3] > r[1];
    const project = (p, r) => [p.x + r[0] * p.sx, p.y + r[1] * p.sy,
        p.x + r[2] * p.sx, p.y + r[3] * p.sy];

    // Clip to overflow ancestors in the flattened tree as well as the visual
    // viewport and frame chain. Hit-testing later also covers clip paths,
    // rounded corners, modal dialogs, and elements occluded by overlays.
    function elementClip(e, p) {
        if (!e.checkVisibility({ opacityProperty: true, visibilityProperty: true, contentVisibilityAuto: true })) return null;
        let clip = p.clip;
        let depth = 0;
        for (let a = e; a; a = composedParent(a)) {
            if (++depth > 96 || a.hasAttribute('inert')) return null;
            if (a === e || a === p.doc.documentElement || a === p.doc.scrollingElement) continue;
            const s = getComputedStyle(a);
            const clipX = s.overflowX !== 'visible';
            const clipY = s.overflowY !== 'visible';
            if ((!clipX && !clipY) || !a.offsetWidth || !a.offsetHeight) continue;
            const b = a.getBoundingClientRect();
            const ax = b.width / a.offsetWidth, ay = b.height / a.offsetHeight;
            const x = b.left + a.clientLeft * ax, y = b.top + a.clientTop * ay;
            const q = project(p, [x, y, x + a.clientWidth * ax, y + a.clientHeight * ay]);
            clip = intersect(clip, [clipX ? q[0] : -Infinity, clipY ? q[1] : -Infinity,
                clipX ? q[2] : Infinity, clipY ? q[3] : Infinity]);
            if (!valid(clip)) return null;
        }
        return clip;
    }

    function frameContext(e, p) {
        if (e.localName !== 'iframe' || !e.contentDocument?.documentElement || !e.offsetWidth || !e.offsetHeight) return null;
        const clip = elementClip(e, p);
        if (!clip) return null;
        // Translation and positive axis-aligned scaling can be represented by
        // our rectangle mapping. Skip rotated, skewed and perspective frames.
        for (let a = e; a; a = composedParent(a)) {
            const s = getComputedStyle(a);
            if (s.rotate !== 'none' || s.perspective !== 'none' || s.offsetPath !== 'none' ||
                (s.scale !== 'none' && s.scale.split(' ').some(v => !(Number(v) > 0)))) return null;
            if (s.transform !== 'none') {
                const m = new DOMMatrixReadOnly(s.transform);
                if (!m.is2D || m.b !== 0 || m.c !== 0 || m.a <= 0 || m.d <= 0) return null;
            }
        }
        const b = e.getBoundingClientRect();
        const s = getComputedStyle(e);
        const ax = b.width / e.offsetWidth, ay = b.height / e.offsetHeight;
        const left = parseFloat(s.paddingLeft), right = parseFloat(s.paddingRight);
        const top = parseFloat(s.paddingTop), bottom = parseFloat(s.paddingBottom);
        const x = b.left + (e.clientLeft + left) * ax, y = b.top + (e.clientTop + top) * ay;
        const w = (e.clientWidth - left - right) * ax, h = (e.clientHeight - top - bottom) * ay;
        const q = project(p, [x, y, x + w, y + h]);
        const clipped = intersect(clip, q);
        const { innerWidth: iw, innerHeight: ih } = e.contentWindow;
        if (!valid(clipped) || !(iw > 0) || !(ih > 0)) return null;
        return { doc: e.contentDocument, owner: e, parent: p, clip: clipped,
            x: q[0], y: q[1], sx: (q[2] - q[0]) / iw, sy: (q[3] - q[1]) / ih };
    }

    function hits(e, p, x, y) {
        const px = (x - p.x) / p.sx, py = (y - p.y) / p.sy;
        // elementFromPoint retargets at a shadow boundary. Validate each host
        // outward so a shadow-root query alone cannot miss an outside overlay.
        let ancestor = e;
        while (ancestor) {
            const root = ancestor.getRootNode();
            let hit = root.elementFromPoint(px, py);
            for (let depth = 0; hit && hit !== ancestor && depth < 96; depth++) hit = composedParent(hit);
            if (hit !== ancestor) return false;
            ancestor = root.host;
        }
        return !p.owner || hits(p.owner, p.parent, x, y);
    }

    function collect(e, p) {
        const clip = elementClip(e, p);
        if (!clip) return false;
        let hit = false;
        const boxes = e.getClientRects();
        for (let i = 0; i < Math.min(8, boxes.length) && rects.size < 64; i++) {
            const r = intersect(clip, project(p, fromRect(boxes[i])));
            if (!valid(r)) continue;
            const w = r[2] - r[0], h = r[3] - r[1];
            const samples = [[.5, .5], [.15, .15], [.85, .15], [.15, .85], [.85, .85]];
            if (!samples.some(([a, b]) => hits(e, p, r[0] + a * w, r[1] + b * h))) continue;
            // Snap inward to prevent pixel rounding from reintroducing clips.
            const l = Math.ceil(wx + r[0]), t = Math.ceil(wy + chromeHeight + r[1]);
            const rgt = Math.floor(wx + r[2]), bot = Math.floor(wy + chromeHeight + r[3]);
            if (l < rgt && t < bot) {
                const out = [l, t, rgt - l, bot - t];
                rects.set(out.join(','), out);
                hit = true;
            }
        }
        return hit;
    }

    // Examine focus separately; large DOMs and the rectangle cap must not
    // starve the active control. Never move focus or request page activation.
    let active = false, hint = 'text';
    if (document.hasFocus()) {
        let p = top, e = document.activeElement, hops = 0;
        while (e && ++hops <= 24) {
            if (e.shadowRoot?.activeElement) { e = e.shadowRoot.activeElement; continue; }
            if (e.localName === 'iframe') {
                p = frameContext(e, p);
                if (!p) break;
                e = p.doc.activeElement;
                continue;
            }
            // Focus may be on a descendant within an editing host.
            while (e.isContentEditable && !e.hasAttribute('contenteditable') && ++hops <= 24) e = composedParent(e);
            const input = purpose(e);
            if (input && collect(e, p)) { active = true; hint = input; }
            break;
        }
    }

    // Bounded incremental tree walk. querySelectorAll would enumerate an
    // unbounded result before a deadline or result cap could be checked.
    let budget = 5000;
    function walk(first, p, depth) {
        if (depth > 32) return;
        for (let e = first; e && budget > 0 && rects.size < 64 && performance.now() < deadline; e = e.nextElementSibling) {
            budget--;
            if (purpose(e)) collect(e, p);
            if (e.shadowRoot) walk(e.shadowRoot.firstElementChild, p, depth + 1);
            if (e.localName === 'iframe') {
                const child = frameContext(e, p);
                if (child) walk(child.doc.documentElement, child, depth + 1);
            }
            walk(e.firstElementChild, p, depth + 1);
        }
    }
    walk(document.documentElement, top, 0);
    return { active, hint, rects: [...rects.values()] };
})
