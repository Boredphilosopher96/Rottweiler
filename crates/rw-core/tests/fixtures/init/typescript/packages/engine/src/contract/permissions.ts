export type PermissionDecision = 'allow' | 'deny' | 'ask'

/** A stored per-origin decision for one Chromium permission. */
export interface PermissionPolicyEntry {
  origin: string
  permission: string
  decision: PermissionDecision
}

/** A stored decision for one external URL scheme (zoommtg, slack, mailto, ...). */
export interface ExternalPolicyEntry {
  scheme: string
  decision: PermissionDecision
}

/** A request awaiting a user answer (site permission, or "open in external app"). */
export interface PendingPrompt {
  id: string
  kind: 'permission' | 'external' | 'httpAuth'
  origin: string
  permission?: string
  scheme?: string
  url?: string
  /** httpAuth: the server's realm string, the host:port, and whether it's a proxy */
  realm?: string
  host?: string
  isProxy?: boolean
}

/** Credentials returned for an httpAuth prompt, or null to cancel the request. */
export interface AuthCredentials {
  username: string
  password: string
}

/**
 * Browser-sane defaults. Benign capabilities are allowed; anything that touches
 * a sensor, the filesystem-ish surface, a device, or the user's attention asks.
 * Unknown permissions default to 'ask' (safe).
 */
export const DEFAULT_PERMISSION: Record<string, PermissionDecision> = {
  fullscreen: 'allow',
  'clipboard-sanitized-write': 'allow',
  pointerLock: 'allow',
  midi: 'allow',
  'persistent-storage': 'allow',
  'background-sync': 'allow',

  media: 'ask',
  geolocation: 'ask',
  notifications: 'ask',
  midiSysex: 'ask',
  'display-capture': 'ask',
  usb: 'ask',
  serial: 'ask',
  hid: 'ask',
  'idle-detection': 'ask',
  'clipboard-read': 'ask',
  'window-management': 'ask',
  'speaker-selection': 'ask',
  'storage-access': 'ask',
  'top-level-storage-access': 'ask'
}

export function defaultPermission(permission: string): PermissionDecision {
  return DEFAULT_PERMISSION[permission] ?? 'ask'
}

/** schemes Sora renders itself and must never hand to the OS as "external". */
export const INTERNAL_SCHEMES = ['http', 'https', 'about', 'data', 'blob', 'file', 'chrome', 'view-source']
