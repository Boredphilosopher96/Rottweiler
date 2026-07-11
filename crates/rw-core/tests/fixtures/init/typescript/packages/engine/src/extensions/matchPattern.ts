// Chrome extension match patterns: <scheme>://<host><path>, plus <all_urls>.
// Ref shape: '*://*.example.com/*'. Used for content-script injection and host
// permission checks. Pure + heavily testable.

export interface ParsedPattern {
  scheme: string      // '*' | 'http' | 'https' | 'file' | 'ftp' | ...
  host: string        // '*' | '*.foo.com' | 'foo.com' | '' (file)
  path: string        // '/*' etc
  all: boolean        // <all_urls>
}

const SCHEME_STAR = new Set(['http', 'https'])

export function parseMatchPattern(pattern: string): ParsedPattern | null {
  if (pattern === '<all_urls>') return { scheme: '*', host: '*', path: '/*', all: true }
  const m = /^(\*|[a-z][a-z0-9+.-]*):\/\/([^/]*)(\/.*)$/i.exec(pattern)
  if (!m) return null
  const scheme = m[1].toLowerCase()
  const host = m[2].toLowerCase()
  const path = m[3]
  // host may be '', '*', '*.domain', or exact; '*' only allowed as full host or leading label
  if (host.includes('*') && host !== '*' && !host.startsWith('*.')) return null
  return { scheme, host, path, all: false }
}

function hostMatches(patternHost: string, host: string): boolean {
  if (patternHost === '*' || patternHost === '') return true
  if (patternHost.startsWith('*.')) {
    const base = patternHost.slice(2)
    return host === base || host.endsWith('.' + base)
  }
  return host === patternHost
}

function globToRegExp(glob: string): RegExp {
  const esc = glob.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*')
  return new RegExp('^' + esc + '$')
}

export function matchesPattern(pattern: string, url: string): boolean {
  const p = parseMatchPattern(pattern)
  if (!p) return false
  let u: URL
  try { u = new URL(url) } catch { return false }
  const scheme = u.protocol.replace(/:$/, '')
  if (p.all) return ['http', 'https', 'ws', 'wss', 'ftp', 'file'].includes(scheme)
  if (p.scheme === '*') { if (!SCHEME_STAR.has(scheme)) return false }
  else if (p.scheme !== scheme) return false
  if (!hostMatches(p.host, u.hostname)) return false
  return globToRegExp(p.path).test(u.pathname + u.search)
}

export function matchesAny(patterns: string[], url: string): boolean {
  return patterns.some((p) => matchesPattern(p, url))
}
