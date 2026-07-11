/**
 * Injected into sensitive pages under SafeZoning "basic". A best-effort, Sora-side
 * layer on top of Chromium's own isolation: it neutralizes a few common
 * content-injection vectors and watches for scripts sneaked in after load. It is
 * deliberately conservative (won't break normal login pages) and experimental.
 */
export const BASIC_GUARD_SCRIPT = `(() => {
  if (window.__soraSafeZone) return 'already';
  window.__soraSafeZone = { blocked: 0, mode: 'basic' };
  // 1) neutralize document.write on a live login page (classic injection vector)
  try {
    const noop = function () { window.__soraSafeZone.blocked++; };
    Object.defineProperty(document, 'write', { value: noop, configurable: false });
    Object.defineProperty(document, 'writeln', { value: noop, configurable: false });
  } catch (e) {}
  // 2) flag & strip <script> elements injected after initial load
  try {
    const mo = new MutationObserver((muts) => {
      for (const m of muts) {
        m.addedNodes && m.addedNodes.forEach((n) => {
          if (n && n.tagName === 'SCRIPT' && !n.__soraOk) {
            n.type = 'javascript/blocked';
            if (n.parentNode) n.parentNode.removeChild(n);
            window.__soraSafeZone.blocked++;
          }
        });
      }
    });
    mo.observe(document.documentElement, { childList: true, subtree: true });
  } catch (e) {}
  // 3) mark password fields so exfil via property getters is at least observable
  try {
    document.querySelectorAll('input[type=password]').forEach((el) => { el.setAttribute('data-sora-zoned', '1'); });
  } catch (e) {}
  return 'armed';
})()`

/** Runs in-page to collect the signals the policy scores. */
export const SIGNALS_SCRIPT = `(() => ({
  url: location.href,
  hasPasswordField: !!document.querySelector('input[type=password]'),
  hasLoginForm: !!document.querySelector('form input[type=password]') ||
    !!document.querySelector('form[action*="login" i], form[action*="signin" i], form[action*="auth" i]'),
  crossOriginForm: Array.prototype.slice.call(document.querySelectorAll('form[action]')).some((f) => {
    try { return new URL(f.getAttribute('action'), location.href).origin !== location.origin } catch (e) { return false }
  })
}))()`
