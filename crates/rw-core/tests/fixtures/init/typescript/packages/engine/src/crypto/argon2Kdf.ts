import { argon2id } from 'hash-wasm'
import type { Kdf } from './vaultCrypto'

/** Production KDF: Argon2id via hash-wasm (pure WASM, no native build). */
export const argon2Kdf: Kdf = async (password, params) => {
  const out = await argon2id({
    password,
    salt: Buffer.from(params.salt, 'base64'),
    parallelism: params.parallelism ?? 1,
    iterations: params.iterations ?? 3,
    memorySize: params.memKiB ?? 65536,   // KiB
    hashLength: 32,
    outputType: 'binary'
  })
  return new Uint8Array(out)
}
