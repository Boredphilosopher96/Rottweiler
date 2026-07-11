import { randomBytes, createCipheriv, createDecipheriv, scryptSync } from 'node:crypto'
import type { KdfParams, KdfAlgo, Sealed, VaultHeader, Credential } from '../contract/vault'

const b64 = (u: Uint8Array): string => Buffer.from(u).toString('base64')
const unb64 = (s: string): Buffer => Buffer.from(s, 'base64')
const enc = new TextEncoder()
const dec = new TextDecoder()

/** A key-derivation function: password + params → 32-byte key. Injectable so the
 *  envelope is testable with scrypt and runs Argon2id in production. */
export type Kdf = (password: string, params: KdfParams) => Promise<Uint8Array>

// ── AEAD (AES-256-GCM) ──
export function seal(key: Uint8Array, plaintext: Uint8Array, aad?: Uint8Array): Sealed {
  const iv = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', key, iv)
  if (aad) cipher.setAAD(aad)
  const ct = Buffer.concat([cipher.update(plaintext), cipher.final()])
  return { iv: b64(iv), ct: b64(ct), tag: b64(cipher.getAuthTag()) }
}

export function open(key: Uint8Array, sealed: Sealed, aad?: Uint8Array): Uint8Array {
  const decipher = createDecipheriv('aes-256-gcm', key, unb64(sealed.iv))
  if (aad) decipher.setAAD(aad)
  decipher.setAuthTag(unb64(sealed.tag))
  return new Uint8Array(Buffer.concat([decipher.update(unb64(sealed.ct)), decipher.final()]))
}

// ── built-in KDF (scrypt) — real, deterministic, used for tests + as a fallback ──
export const scryptKdf: Kdf = async (password, params) => {
  const salt = unb64(params.salt)
  return new Uint8Array(scryptSync(password, salt, 32, {
    N: params.N ?? 16384, r: params.r ?? 8, p: params.p ?? 1, maxmem: 128 * 1024 * 1024
  }))
}

export function freshKdfParams(algo: KdfAlgo): KdfParams {
  const salt = b64(randomBytes(16))
  return algo === 'argon2id'
    ? { algo, salt, memKiB: 65536, iterations: 3, parallelism: 1 }
    : { algo, salt, N: 16384, r: 8, p: 1 }
}

const CHECK_PLAINTEXT = enc.encode('sora-vault-ok')

// ── envelope operations ──
export interface UnlockedVault {
  header: VaultHeader
  masterKey: Uint8Array   // in memory only
}

/** Create a brand-new vault: random master key, wrapped under KEK(password). */
export async function createVault(password: string, kdf: Kdf, algo: KdfAlgo): Promise<UnlockedVault> {
  const kdfParams = freshKdfParams(algo)
  const kek = await kdf(password, kdfParams)
  const masterKey = new Uint8Array(randomBytes(32))
  const header: VaultHeader = {
    version: 1,
    kdf: kdfParams,
    wrappedMasterKey: seal(kek, masterKey),
    check: seal(masterKey, CHECK_PLAINTEXT)
  }
  return { header, masterKey }
}

/** Unlock: derive KEK, unwrap the master key. Throws on wrong password (GCM auth fail). */
export async function unlockVault(header: VaultHeader, password: string, kdf: Kdf): Promise<Uint8Array> {
  const kek = await kdf(password, header.kdf)
  let masterKey: Uint8Array
  try {
    masterKey = open(kek, header.wrappedMasterKey)
  } catch {
    throw new Error('invalid password')
  }
  // verify master key against the check blob (defends against a swapped wrapped key)
  try { open(masterKey, header.check) } catch { throw new Error('vault integrity check failed') }
  return masterKey
}

/** Change the user password: re-wrap the SAME master key under a new KEK. Entries untouched. */
export async function changePassword(header: VaultHeader, oldPw: string, newPw: string, kdf: Kdf): Promise<VaultHeader> {
  const masterKey = await unlockVault(header, oldPw, kdf)
  const kdfParams = freshKdfParams(header.kdf.algo)
  const kek = await kdf(newPw, kdfParams)
  return { ...header, kdf: kdfParams, wrappedMasterKey: seal(kek, masterKey), check: seal(masterKey, CHECK_PLAINTEXT) }
}

// ── entry encryption (under the master key) ──
export function sealCredential(masterKey: Uint8Array, cred: Credential): Sealed {
  return seal(masterKey, enc.encode(JSON.stringify(cred)))
}
export function openCredential(masterKey: Uint8Array, sealed: Sealed): Credential {
  return JSON.parse(dec.decode(open(masterKey, sealed))) as Credential
}

/**
 * Rotate the master key (opt-in; O(entries)). Decrypts every entry with the old
 * master key and re-encrypts under a fresh one, then re-wraps the new master key.
 * Returns the new header + rewritten sealed blobs (mapped by entry id).
 */
export async function rotateMasterKey(
  header: VaultHeader,
  password: string,
  entries: { id: string; secret: Sealed }[],
  kdf: Kdf
): Promise<{ header: VaultHeader; resealed: Map<string, Sealed> }> {
  const oldMaster = await unlockVault(header, password, kdf)
  const newMaster = new Uint8Array(randomBytes(32))
  const resealed = new Map<string, Sealed>()
  for (const e of entries) {
    const plain = open(oldMaster, e.secret)     // decrypt with old
    resealed.set(e.id, seal(newMaster, plain))  // re-encrypt with new
  }
  const kek = await kdf(password, header.kdf)   // same password, existing KEK
  const newHeader: VaultHeader = {
    ...header,
    wrappedMasterKey: seal(kek, newMaster),
    check: seal(newMaster, CHECK_PLAINTEXT)
  }
  return { header: newHeader, resealed }
}
