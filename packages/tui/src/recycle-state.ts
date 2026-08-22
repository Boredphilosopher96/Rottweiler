import {
  closeSync,
  fchmodSync,
  fsyncSync,
  lstatSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs"
import { dirname } from "node:path"

const MAX_RECYCLE_STATE_BYTES = 8 * 1024 * 1024

export interface TuiRecycleState {
  readonly schemaVersion: 1
  readonly draft: string
  readonly scrollTop: number
}

/** Consume a private, one-shot TUI recycle handoff. Invalid files fail closed. */
export function readTuiRecycleState(path: string | undefined): TuiRecycleState | null {
  if (path === undefined || path.length === 0) return null
  try {
    const metadata = lstatSync(path)
    if (
      metadata.isSymbolicLink() ||
      !metadata.isFile() ||
      metadata.size > MAX_RECYCLE_STATE_BYTES ||
      (metadata.mode & 0o077) !== 0 ||
      (process.getuid !== undefined && metadata.uid !== process.getuid())
    ) return null
    const parsed: unknown = JSON.parse(readFileSync(path, "utf8"))
    unlinkSync(path)
    return parseTuiRecycleState(parsed)
  } catch {
    return null
  }
}

/** Atomically persist the small TUI-only state lost during an RSS recycle. */
export function writeTuiRecycleState(path: string | undefined, state: TuiRecycleState): boolean {
  if (path === undefined || path.length === 0) return false
  const encoded = `${JSON.stringify(state)}\n`
  if (Buffer.byteLength(encoded) > MAX_RECYCLE_STATE_BYTES) return false
  const temporary = `${path}.${process.pid}.tmp`
  let descriptor: number | null = null
  try {
    const parent = lstatSync(dirname(path))
    if (
      parent.isSymbolicLink() ||
      !parent.isDirectory() ||
      (parent.mode & 0o077) !== 0 ||
      (process.getuid !== undefined && parent.uid !== process.getuid())
    ) return false
    descriptor = openSync(temporary, "wx", 0o600)
    fchmodSync(descriptor, 0o600)
    writeFileSync(descriptor, encoded, "utf8")
    fsyncSync(descriptor)
    closeSync(descriptor)
    descriptor = null
    renameSync(temporary, path)
    return true
  } catch {
    return false
  } finally {
    if (descriptor !== null) closeSync(descriptor)
    try {
      unlinkSync(temporary)
    } catch {
      // The successful rename consumed it; a failed best-effort cleanup is inert.
    }
  }
}

export function parseTuiRecycleState(value: unknown): TuiRecycleState | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null
  const candidate = value as Record<string, unknown>
  if (
    candidate.schemaVersion !== 1 ||
    typeof candidate.draft !== "string" ||
    !Number.isSafeInteger(candidate.scrollTop) ||
    (candidate.scrollTop as number) < 0
  ) return null
  return {
    schemaVersion: 1,
    draft: candidate.draft,
    scrollTop: candidate.scrollTop as number,
  }
}
