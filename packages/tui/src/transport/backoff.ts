export interface BackoffPolicy {
  readonly initialDelayMs: number
  readonly maximumDelayMs: number
  readonly multiplier: number
  readonly maximumAttempts?: number
}

export interface BackoffScheduler {
  sleep(delayMs: number, signal: AbortSignal): Promise<void>
}

export const DEFAULT_BACKOFF_POLICY: BackoffPolicy = {
  initialDelayMs: 100,
  maximumDelayMs: 5_000,
  multiplier: 2,
}

export const systemBackoffScheduler: BackoffScheduler = {
  sleep(delayMs, signal) {
    if (signal.aborted) {
      return Promise.reject(signal.reason ?? new DOMException("Aborted", "AbortError"))
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        signal.removeEventListener("abort", onAbort)
        resolve()
      }, delayMs)
      const onAbort = () => {
        clearTimeout(timer)
        reject(signal.reason ?? new DOMException("Aborted", "AbortError"))
      }
      signal.addEventListener("abort", onAbort, { once: true })
    })
  },
}

export function backoffDelay(policy: BackoffPolicy, attempt: number): number {
  if (!Number.isSafeInteger(attempt) || attempt < 0) {
    throw new RangeError("backoff attempt must be a non-negative safe integer")
  }
  const delay = policy.initialDelayMs * policy.multiplier ** attempt
  return Math.min(policy.maximumDelayMs, Math.max(0, Math.floor(delay)))
}

export function validateBackoffPolicy(policy: BackoffPolicy): void {
  if (
    !Number.isFinite(policy.initialDelayMs) ||
    policy.initialDelayMs < 0 ||
    !Number.isFinite(policy.maximumDelayMs) ||
    policy.maximumDelayMs < policy.initialDelayMs ||
    !Number.isFinite(policy.multiplier) ||
    policy.multiplier < 1 ||
    (policy.maximumAttempts !== undefined &&
      (!Number.isSafeInteger(policy.maximumAttempts) || policy.maximumAttempts < 0))
  ) {
    throw new RangeError("invalid reconnect backoff policy")
  }
}
