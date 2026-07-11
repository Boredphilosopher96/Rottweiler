// SafeZoning (experimental) — extra Sora-side hardening on sensitive pages
// (logins, payment, auth). Chromium already isolates a lot; this adds a belt-
// and-suspenders layer for the security-curious. Two levels:
//   basic  — inject a content-injection guard into sensitive pages
//   forced — additionally run the sensitive interaction in an isolated context
//            and return only the finished result to the tab

export type SafeZoningMode = 'off' | 'basic' | 'forced'
export type ZoningAction = 'allow' | 'harden' | 'isolate'

export interface PageSignals {
  url: string
  hasPasswordField: boolean
  hasLoginForm: boolean
  crossOriginForm: boolean       // a form posting to another origin
}

export interface ZoningDecision {
  action: ZoningAction
  sensitive: boolean
  score: number                  // 0..100 sensitivity
  reasons: string[]
}

export interface SafeZoningState {
  mode: SafeZoningMode
  experimental: true
  activeZones: number            // pages currently hardened/isolated
  lastDecision: ZoningDecision | null
}
