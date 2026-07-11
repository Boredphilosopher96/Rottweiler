import type { SecurityInfo, SecurityLevel } from '../contract/security'

const INTERNAL_SCHEMES = new Set(['sora:', 'about:', 'file:', 'devtools:', 'chrome:', 'data:', 'blob:'])

export interface SecurityInput {
  url: string
  certError?: string | null
  mixedContent?: boolean
}

export function classifySecurity(input: SecurityInput): SecurityInfo {
  const certError = input.certError ?? null
  const mixedContent = Boolean(input.mixedContent)
  let scheme = ''
  let host = ''
  try {
    const u = new URL(input.url)
    scheme = u.protocol; host = u.hostname
  } catch {
    // URL() rejects some valid-but-messy urls (unencoded data: etc); recover the scheme
    const m = /^([a-z][a-z0-9+.-]*):/i.exec(input.url || '')
    if (m) scheme = m[1].toLowerCase() + ':'
  }

  const encrypted = scheme === 'https:' || scheme === 'wss:'
  let level: SecurityLevel
  if (INTERNAL_SCHEMES.has(scheme)) level = 'internal'
  else if (certError) level = 'dangerous'
  else if (encrypted && mixedContent) level = 'mixed'
  else if (encrypted) level = 'secure'
  else if (scheme === 'http:' || scheme === 'ws:') level = 'insecure'
  else level = 'unknown'

  const summary = {
    secure: 'Connection is secure',
    mixed: 'Secure page loading insecure content',
    insecure: 'Connection is not secure',
    dangerous: certError ? `Certificate problem: ${certError}` : 'Connection is not private',
    internal: 'Sora internal page',
    unknown: 'Security status unknown'
  }[level]

  return { level, scheme: scheme.replace(/:$/, ''), host, encrypted, certError, mixedContent, summary }
}
