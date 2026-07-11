import type { SafeZoningMode, ZoningAction, PageSignals, ZoningDecision } from '../contract/safeZoning'

const AUTH_PATH = /(^|\/)(login|log-in|signin|sign-in|auth|oauth|sso|account|checkout|payment|billing|wallet|password)(\/|$|\?)/i
const AUTH_HOST = /(^|\.)(login|auth|accounts|secure|id|signin)\./i

/** Score how "sensitive" a page looks (0..100) with human-readable reasons. */
export function classifyPage(sig: PageSignals): { sensitive: boolean; score: number; reasons: string[] } {
  const reasons: string[] = []
  let score = 0
  if (sig.hasPasswordField) { score += 55; reasons.push('password field present') }
  if (sig.hasLoginForm) { score += 25; reasons.push('login form detected') }
  let host = '', path = ''
  try { const u = new URL(sig.url); host = u.hostname; path = u.pathname + u.search } catch { /* ignore */ }
  if (AUTH_PATH.test(path)) { score += 20; reasons.push('auth-like URL path') }
  if (AUTH_HOST.test(host)) { score += 15; reasons.push('auth-like hostname') }
  if (sig.crossOriginForm) { score += 20; reasons.push('form posts cross-origin') }
  score = Math.min(100, score)
  return { sensitive: score >= 50, score, reasons }
}

/** Given the user's mode + page signals, decide what to do. Pure + testable. */
export function decideZoning(mode: SafeZoningMode, sig: PageSignals): ZoningDecision {
  const { sensitive, score, reasons } = classifyPage(sig)
  let action: ZoningAction = 'allow'
  if (mode === 'basic') action = sensitive ? 'harden' : 'allow'
  else if (mode === 'forced') action = sensitive ? 'isolate' : 'harden'
  return { action, sensitive, score, reasons }
}
