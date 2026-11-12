// Baseline under test: a document-start JS override script of the kind commonly
// injected with Page.addScriptToEvaluateOnNewDocument. Reproduced verbatim in
// behaviour so detectors see exactly what such a script exposes. Placeholders
// (__NAME__) are substituted by engines.py from the per-seed fingerprint.
(() => {
  'use strict';
    Object.defineProperty(navigator, 'webdriver', { get: () => false, configurable: true });
    Object.defineProperty(navigator, 'languages', { get: () => __LANGS__, configurable: true });
    Object.defineProperty(navigator, 'language', { get: () => __LANG__, configurable: true });
    Object.defineProperty(navigator, 'platform', { get: () => 'Linux x86_64', configurable: true });
    Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => __CORES__, configurable: true });
    Object.defineProperty(navigator, 'deviceMemory', { get: () => __MEM__, configurable: true });
    Object.defineProperty(navigator, 'pdfViewerEnabled', { get: () => true, configurable: true });
    if (!window.chrome) { window.chrome = {}; }
    if (!window.chrome.runtime) {
      window.chrome.runtime = {
        OnInstalledReason: { CHROME_UPDATE: 'chrome_update', INSTALL: 'install', SHARED_MODULE_UPDATE: 'shared_module_update', UPDATE: 'update' },
        OnRestartRequiredReason: { APP_UPDATE: 'app_update', OS_UPDATE: 'os_update', PERIODIC: 'periodic' },
        PlatformArch: { ARM: 'arm', ARM64: 'arm64', MIPS: 'mips', MIPS64: 'mips64', X86_32: 'x86-32', X86_64: 'x86-64' },
        PlatformOs: { ANDROID: 'android', CROS: 'cros', LINUX: 'linux', MAC: 'mac', OPENBSD: 'openbsd', WIN: 'win' },
        RequestUpdateCheckStatus: { NO_UPDATE: 'no_update', THROTTLED: 'throttled', UPDATE_AVAILABLE: 'update_available' }
      };
    }
    window.chrome.app = window.chrome.app || { isInstalled: false, InstallState: { DISABLED: 'disabled', INSTALLED: 'installed', NOT_INSTALLED: 'not_installed' }, RunningState: { CANNOT_RUN: 'cannot_run', READY_TO_RUN: 'ready_to_run', RUNNING: 'running' } };
    const patchWebGL = (proto) => {
      if (!proto || !proto.getParameter) return;
      const origParam = proto.getParameter;
      proto.getParameter = function(param) {
        if (param === 37445) return __VENDOR__;
        if (param === 37446) return __RENDERER__;
        return origParam.apply(this, arguments);
      };
      const origExt = proto.getExtension;
      if (origExt) {
        proto.getExtension = function(name) {
          const res = origExt.apply(this, arguments);
          if (name === 'WEBGL_debug_renderer_info' && !res) {
            return { UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 };
          }
          return res;
        };
      }
    };
    if (typeof WebGLRenderingContext !== 'undefined') patchWebGL(WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== 'undefined') patchWebGL(WebGL2RenderingContext.prototype);
    if (typeof HTMLCanvasElement !== 'undefined') {
      const origGetContext = HTMLCanvasElement.prototype.getContext;
      HTMLCanvasElement.prototype.getContext = function(type, attrs) {
        const ctx = origGetContext.apply(this, arguments);
        if (!ctx && (type === 'webgl' || type === 'experimental-webgl' || type === 'webgl2')) {
          return {
            UNMASKED_VENDOR_WEBGL: 37445,
            UNMASKED_RENDERER_WEBGL: 37446,
            getExtension: (n) => n === 'WEBGL_debug_renderer_info' ? { UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 } : null,
            getParameter: (p) => p === 37445 ? __VENDOR__ : (p === 37446 ? __RENDERER__ : 'WebGL 1.0'),
            getSupportedExtensions: () => ['WEBGL_debug_renderer_info', 'OES_texture_float', 'OES_element_index_uint'],
          };
        }
        return ctx;
      };
    }
    if (navigator.permissions && navigator.permissions.query) {
      const origQuery = navigator.permissions.query.bind(navigator.permissions);
      navigator.permissions.query = (params) => (
        params && params.name === 'notifications'
          ? Promise.resolve({ state: Notification.permission, onchange: null })
          : origQuery(params)
      );
    }
    if (typeof HTMLCanvasElement !== 'undefined') {
      const origGetImageData = CanvasRenderingContext2D.prototype.getImageData;
      CanvasRenderingContext2D.prototype.getImageData = function() {
        const imageData = origGetImageData.apply(this, arguments);
        if (imageData && imageData.data && imageData.data.length >= 16) {
          imageData.data[0] = (imageData.data[0] ^ (__NOISE__ & 1));
        }
        return imageData;
      };
    }
  return true;
})();
