import { writeFile } from "node:fs/promises"
import { join } from "node:path"

export const MAX_NATIVE_RICH_REPORT_BYTES = 1024 * 1024
const MAX_FAILURE_BYTES = 4096

interface NativeRichOwners {
  readonly releaseRelay: () => Promise<void>
  readonly closeClient: () => Promise<void>
  readonly finalEvidence: () => Record<string, unknown>
}

function utf8Prefix(value: string, maximumBytes: number): string {
  if (Buffer.byteLength(value) <= maximumBytes) return value
  let low = 0
  let high = Math.min(value.length, maximumBytes)
  while (low < high) {
    const middle = Math.ceil((low + high) / 2)
    if (Buffer.byteLength(value.slice(0, middle)) <= maximumBytes) low = middle
    else high = middle - 1
  }
  if (low > 0 && /[\uD800-\uDBFF]/.test(value[low - 1] ?? "")) low -= 1
  return Buffer.from(value.slice(0, low), "utf8").toString("utf8")
}

function failureText(error: unknown): string {
  return utf8Prefix(error instanceof Error ? error.message : String(error), MAX_FAILURE_BYTES)
}

function failureDetails(error: unknown): string[] {
  if (!(error instanceof AggregateError)) return [failureText(error)]
  const details = [...error.errors].slice(0, 16).map(failureText)
  return details.length === 0 ? [failureText(error)] : details
}

/** Settle both native owners and publish bounded evidence before propagating the first failure. */
export async function finishNativeRichProbe(
  directory: string,
  evidence: Record<string, unknown>,
  functionalFailure: unknown,
  owners: NativeRichOwners,
): Promise<void> {
  const cleanupCauses: unknown[] = []
  const cleanupFailures: string[] = []
  for (const [label, cleanup] of [
    ["relay release", owners.releaseRelay],
    ["connected client", owners.closeClient],
  ] as const) {
    try { await cleanup() }
    catch (error) {
      cleanupCauses.push(error)
      cleanupFailures.push(...failureDetails(error).map(detail => `${label}: ${detail}`))
    }
  }

  let reportFailure: unknown
  let finalEvidence: Record<string, unknown> = {}
  try { finalEvidence = owners.finalEvidence() }
  catch (error) { reportFailure = error }
  const firstFailure = functionalFailure === undefined ? null : failureText(functionalFailure)
  let report: Record<string, unknown> = {
    schemaVersion: 1,
    pid: process.pid,
    ...evidence,
    ...finalEvidence,
    failure: firstFailure,
    cleanupFailures,
    passed: functionalFailure === undefined && cleanupCauses.length === 0 && reportFailure === undefined,
  }
  let encoded: Buffer
  try {
    encoded = Buffer.from(JSON.stringify(report) + "\n")
    if (encoded.byteLength > MAX_NATIVE_RICH_REPORT_BYTES) {
      throw new Error(`native rich report exceeds ${MAX_NATIVE_RICH_REPORT_BYTES} bytes`)
    }
  } catch (error) {
    reportFailure ??= error
    report = {
      schemaVersion: 1,
      pid: process.pid,
      failure: firstFailure,
      cleanupFailures,
      reportFailure: failureText(reportFailure),
      passed: false,
    }
    encoded = Buffer.from(JSON.stringify(report) + "\n")
  }

  let writeFailure: unknown
  try { await writeFile(join(directory, "native-rich.json"), encoded, { mode: 0o600 }) }
  catch (error) { writeFailure = error }
  if (functionalFailure !== undefined) throw functionalFailure
  const finalFailures = [...cleanupCauses]
  if (reportFailure !== undefined) finalFailures.push(reportFailure)
  if (writeFailure !== undefined) finalFailures.push(writeFailure)
  if (finalFailures.length > 0) throw new AggregateError(finalFailures, "native rich probe settlement failed")
}
