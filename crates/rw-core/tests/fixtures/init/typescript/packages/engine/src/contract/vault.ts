// Sora password vault — envelope encryption.
//
//   user password ──Argon2id──▶ KEK ──wraps──▶ Master Key ──encrypts──▶ entries
//
// The user password never touches an entry. Changing it re-wraps ONE key (the
// master key). Rotating the master key re-encrypts every entry (opt-in; costly).

export type KdfAlgo = 'argon2id' | 'scrypt'

export interface KdfParams {
  algo: KdfAlgo
  salt: string            // base64
  // argon2id
  memKiB?: number
  iterations?: number
  parallelism?: number
  // scrypt (test / fallback)
  N?: number
  r?: number
  p?: number
}

/** AES-256-GCM sealed blob, all base64. */
export interface Sealed {
  iv: string
  ct: string
  tag: string
}

/** Non-secret vault header, safe to persist in the clear. */
export interface VaultHeader {
  version: 1
  kdf: KdfParams
  wrappedMasterKey: Sealed    // master key sealed under the KEK
  check: Sealed               // a known plaintext sealed under the master key (fast unlock verification)
}

/** One saved login. `origin` is clear (for matching); credentials are sealed. */
export interface VaultEntry {
  id: string
  origin: string              // e.g. https://example.com
  label: string               // display hint (host), clear
  secret: Sealed              // seals { username, password, notes, totp? } under the master key
  createdAt: number
  updatedAt: number
}

/** Decrypted credential payload (only in memory while unlocked). */
export interface Credential {
  username: string
  password: string
  notes?: string
  totp?: string
}

/** State surfaced to the UI. Never contains secrets or the master key. */
export interface VaultState {
  exists: boolean
  locked: boolean
  entryCount: number
  kdfAlgo: KdfAlgo
  rotateOnUnlock: boolean      // the opt-in "rotate encryption" preference
  lastRotated: number | null
  entries: VaultEntryMeta[]    // metadata only (no secrets); [] while locked
}

export interface VaultEntryMeta {
  id: string
  origin: string
  label: string
  username: string             // populated only while unlocked
  updatedAt: number
}
