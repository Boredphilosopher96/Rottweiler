/** Coarse security posture of the active page, for the omnibox badge + detail panel. */
export type SecurityLevel =
  | 'secure'       // https, valid cert, no mixed content
  | 'mixed'        // https page pulling insecure subresources
  | 'insecure'     // plain http
  | 'dangerous'    // cert error / known-bad
  | 'internal'     // sora://, about:, file:, devtools — chrome-ish, not web
  | 'unknown'

export interface SecurityInfo {
  level: SecurityLevel
  scheme: string
  host: string
  /** true when the transport is TLS (https/wss) */
  encrypted: boolean
  certError: string | null
  mixedContent: boolean
  summary: string
}
