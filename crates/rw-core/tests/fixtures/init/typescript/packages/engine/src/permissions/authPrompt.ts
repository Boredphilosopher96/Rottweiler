import type { PendingPrompt } from '@shared/permissions'

/** Electron's login `authInfo` shape (the bits we use). */
export interface AuthInfo {
  isProxy?: boolean
  scheme?: string
  host?: string
  port?: number
  realm?: string
}

/** Build the httpAuth prompt base (sans id) from Electron's login event data. */
export function buildAuthPromptBase(authInfo: AuthInfo, url: string): Omit<PendingPrompt, 'id'> {
  const host = authInfo.host || (() => { try { return new URL(url).host } catch { return '' } })()
  let origin = ''
  try { origin = new URL(url).origin } catch { origin = host }
  return {
    kind: 'httpAuth',
    origin,
    host,
    url,
    realm: authInfo.realm || '',
    isProxy: Boolean(authInfo.isProxy)
  }
}
