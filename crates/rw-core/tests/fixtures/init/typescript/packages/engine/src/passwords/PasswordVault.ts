import { EventEmitter } from 'node:events'
import { randomUUID } from 'node:crypto'
import { Store } from '../persistence/Store'
import * as vc from '../crypto/vaultCrypto'
import { matchEntriesForOrigin, originOf } from './autofill'
import type { VaultHeader, VaultEntry, VaultState, VaultEntryMeta, Credential, KdfAlgo } from '../contract/vault'

interface VaultData {
  header: VaultHeader | null
  entries: VaultEntry[]
  rotateOnUnlock: boolean
  lastRotated: number | null
}

const EMPTY: VaultData = { header: null, entries: [], rotateOnUnlock: false, lastRotated: null }

/**
 * Sora password vault. Holds the sealed header + entries on disk; the master key
 * lives in memory only while unlocked. All crypto goes through vaultCrypto with
 * the injected KDF (Argon2id in production).
 */
export class PasswordVault extends EventEmitter {
  private store: Store<VaultData>
  private masterKey: Uint8Array | null = null

  constructor(dir: string, private readonly kdf: vc.Kdf) {
    super()
    this.store = new Store<VaultData>(dir, 'vault', EMPTY)
  }

  exists(): boolean { return this.store.get().header !== null }
  locked(): boolean { return this.masterKey === null }

  private data(): VaultData { return this.store.get() }
  private save(d: VaultData): void { this.store.replace(d); this.emit('changed') }

  /** Create a new vault and leave it unlocked. */
  async create(password: string, algo: KdfAlgo = 'argon2id'): Promise<void> {
    if (this.exists()) throw new Error('vault already exists')
    const v = await vc.createVault(password, this.kdf, algo)
    this.masterKey = v.masterKey
    this.save({ ...EMPTY, header: v.header })
  }

  /** Unlock with the user password. Returns false on wrong password. */
  async unlock(password: string): Promise<boolean> {
    const d = this.data()
    if (!d.header) return false
    try {
      this.masterKey = await vc.unlockVault(d.header, password, this.kdf)
    } catch {
      this.masterKey = null
      return false
    }
    if (d.rotateOnUnlock) await this.rotate(password)
    else this.emit('changed')
    return true
  }

  lock(): void {
    if (this.masterKey) this.masterKey.fill(0)
    this.masterKey = null
    this.emit('changed')
  }

  private requireKey(): Uint8Array {
    if (!this.masterKey) throw new Error('vault is locked')
    return this.masterKey
  }

  async changePassword(oldPw: string, newPw: string): Promise<boolean> {
    const d = this.data()
    if (!d.header) return false
    try {
      const header = await vc.changePassword(d.header, oldPw, newPw, this.kdf)
      this.save({ ...d, header })
      return true
    } catch { return false }
  }

  setRotateOnUnlock(on: boolean): void {
    this.save({ ...this.data(), rotateOnUnlock: on })
  }

  /** Manually rotate the master key (opt-in; O(entries)). Requires the password. */
  async rotate(password: string): Promise<boolean> {
    const d = this.data()
    if (!d.header) return false
    try {
      const { header, resealed } = await vc.rotateMasterKey(
        d.header, password, d.entries.map((e) => ({ id: e.id, secret: e.secret })), this.kdf
      )
      const entries = d.entries.map((e) => resealed.has(e.id) ? { ...e, secret: resealed.get(e.id)! } : e)
      this.masterKey = await vc.unlockVault(header, password, this.kdf)
      this.save({ ...d, header, entries, lastRotated: Date.now() })
      return true
    } catch { return false }
  }

  // ── entries ──
  addEntry(url: string, cred: Credential): VaultEntry {
    const key = this.requireKey()
    const origin = originOf(url) ?? url
    const now = Date.now()
    const entry: VaultEntry = {
      id: randomUUID(),
      origin,
      label: (() => { try { return new URL(origin).hostname } catch { return origin } })(),
      secret: vc.sealCredential(key, cred),
      createdAt: now,
      updatedAt: now
    }
    const d = this.data()
    this.save({ ...d, entries: [entry, ...d.entries] })
    return entry
  }

  updateEntry(id: string, cred: Credential): void {
    const key = this.requireKey()
    const d = this.data()
    this.save({ ...d, entries: d.entries.map((e) => e.id === id ? { ...e, secret: vc.sealCredential(key, cred), updatedAt: Date.now() } : e) })
  }

  removeEntry(id: string): void {
    const d = this.data()
    this.save({ ...d, entries: d.entries.filter((e) => e.id !== id) })
  }

  getCredential(id: string): Credential | null {
    const key = this.requireKey()
    const e = this.data().entries.find((x) => x.id === id)
    if (!e) return null
    try { return vc.openCredential(key, e.secret) } catch { return null }
  }

  /** Autofill candidates for a page, with usernames decrypted (requires unlock). */
  match(url: string): VaultEntryMeta[] {
    if (this.locked()) return []
    return matchEntriesForOrigin(this.data().entries, url).map((e) => this.meta(e))
  }

  private meta(e: VaultEntry): VaultEntryMeta {
    let username = ''
    if (this.masterKey) { try { username = vc.openCredential(this.masterKey, e.secret).username } catch { /* skip */ } }
    return { id: e.id, origin: e.origin, label: e.label, username, updatedAt: e.updatedAt }
  }

  state(): VaultState {
    const d = this.data()
    return {
      exists: d.header !== null,
      locked: this.locked(),
      entryCount: d.entries.length,
      kdfAlgo: d.header?.kdf.algo ?? 'argon2id',
      rotateOnUnlock: d.rotateOnUnlock,
      lastRotated: d.lastRotated,
      entries: this.locked() ? [] : d.entries.map((e) => this.meta(e))
    }
  }
}
