import { scryptKdf, type Kdf } from './vaultCrypto'
import { argon2Kdf } from './argon2Kdf'

/** Dispatches to the KDF named in the vault's stored params, so unlock always
 *  uses the algorithm the vault was created/rotated with. */
export const dispatchKdf: Kdf = (password, params) =>
  params.algo === 'scrypt' ? scryptKdf(password, params) : argon2Kdf(password, params)
