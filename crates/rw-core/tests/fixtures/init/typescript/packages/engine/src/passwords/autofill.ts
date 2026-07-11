import type { VaultEntry } from '../contract/vault'

/** Normalize a page URL to an origin ('https://host[:port]'), or null. */
export function originOf(url: string): string | null {
  try {
    const u = new URL(url)
    if (u.protocol !== 'https:' && u.protocol !== 'http:') return null
    return u.origin
  } catch { return null }
}

/** Registrable-domain heuristic (last two labels). Not a full PSL — good enough
 *  to let login.example.com share example.com's saved logins. */
export function registrableDomain(host: string): string {
  const parts = host.split('.').filter(Boolean)
  if (parts.length <= 2) return host
  return parts.slice(-2).join('.')
}

function hostOf(origin: string): string {
  try { return new URL(origin).hostname } catch { return '' }
}

/**
 * Candidate entries for autofill on a page. Exact-origin matches rank first, then
 * same registrable-domain matches (relaxed). Most-recently-updated within a tier wins.
 */
export function matchEntriesForOrigin(entries: VaultEntry[], url: string, relaxSubdomains = true): VaultEntry[] {
  const origin = originOf(url)
  if (!origin) return []
  const host = hostOf(origin)
  const rd = registrableDomain(host)
  const exact: VaultEntry[] = []
  const relaxed: VaultEntry[] = []
  for (const e of entries) {
    if (e.origin === origin) { exact.push(e); continue }
    if (relaxSubdomains) {
      const eh = hostOf(e.origin)
      if (eh && registrableDomain(eh) === rd) relaxed.push(e)
    }
  }
  const byRecent = (a: VaultEntry, b: VaultEntry) => b.updatedAt - a.updatedAt
  return [...exact.sort(byRecent), ...relaxed.sort(byRecent)]
}
