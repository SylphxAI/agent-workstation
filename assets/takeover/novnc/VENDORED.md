noVNC 1.7.0, vendored from https://github.com/novnc/noVNC (tag v1.7.0):
`core/` and `vendor/pako/`. The noVNC core is licensed under MPL-2.0
(docs/LICENSE.MPL-2.0), pako under MIT (vendor/pako/LICENSE); see LICENSE.txt
and AUTHORS. Source: https://github.com/novnc/noVNC/tree/v1.7.0

Local change: core/rfb.js exposes sendPointer(x, y, mask) for an external
viewport. It takes bounded framebuffer coordinates and honors viewOnly.
When cursorTarget is supplied, cursor shapes use native CSS on that target.
An unavailable shape falls back to the browser's default cursor without a
positioned canvas overlay.
