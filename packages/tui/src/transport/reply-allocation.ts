/** Caller-owned reply memory remains admitted through validation and consumer transfer. */
export interface ReplyAllocation {
  admit(bytes: number): void
}

/** Counts allocation shape without materializing keys, strings, containers, or values. */
export class JsonAllocationShape {
  #string = false
  #escape = false
  #primitive = false
  #depth = 0
  #overhead = 0
  #bytes = 0

  append(chunk: Uint8Array): void {
    this.#bytes += chunk.length
    for (const byte of chunk) {
      if (this.#string) {
        if (this.#escape) this.#escape = false
        else if (byte === 92) this.#escape = true
        else if (byte === 34) this.#string = false
        continue
      }
      if (byte === 34) { this.#string = true; this.#primitive = false; this.#overhead += 24 }
      else if (byte === 123 || byte === 91) {
        if (++this.#depth > 64) throw new Error("reply exceeds the supported JSON nesting depth")
        this.#overhead += byte === 123 ? 48 : 40
        this.#primitive = false
      } else if (byte === 125 || byte === 93) { this.#depth--; this.#primitive = false }
      else if (byte === 58) { this.#overhead += 48; this.#primitive = false }
      else if (byte === 44) { this.#overhead += 8; this.#primitive = false }
      else if (byte <= 32) this.#primitive = false
      else if (!this.#primitive) { this.#primitive = true; this.#overhead += 8 }
    }
  }

  /** Source buffer, UTF-16 decode, retained graph and bounded envelope bookkeeping. */
  peak(bufferCapacity: number, previousCapacity: number): number {
    return Math.max(bufferCapacity + previousCapacity, bufferCapacity + this.#bytes * 4 + this.#overhead) + 2048
  }
}
