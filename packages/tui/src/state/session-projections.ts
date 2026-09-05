import {
  type RottweilerState
} from "./model"

export function providerQualifiedRoute(
  value: string | null | undefined,
): { readonly provider: string; readonly model: string } | null {
  if (value === null || value === undefined) return null
  const separator = value.indexOf("/")
  if (separator <= 0 || separator === value.length - 1) return null
  return { provider: value.slice(0, separator), model: value }
}

export function projectSession(session: {
  readonly session_id: string
  readonly title?: string
  readonly workspace_name: string
  readonly model: string
  readonly driver_client_id?: string | null
  readonly shell_active: boolean
}): RottweilerState["sessions"][number] {
  return {
    sessionId: session.session_id,
    ...(session.title ? { title: session.title } : {}),
    workspaceName: session.workspace_name,
    model: session.model,
    driverClientId: session.driver_client_id ?? null,
    shellActive: session.shell_active,
  }
}

export function projectSessionReview(review: {
  readonly session_id: string
  readonly files: readonly {
    readonly path: string
    readonly unified_diff: string
    readonly status: "pending" | "accepted" | "reverted"
    readonly truncated: boolean
    readonly unrestorable_reason?: string | null
    readonly original_hash: string
    readonly current_hash: string
  }[]
}): NonNullable<RottweilerState["review"]> {
  return {
    sessionId: review.session_id,
    files: review.files.map((file) => ({
      path: file.path,
      unifiedDiff: file.unified_diff,
      status: file.status,
      truncated: file.truncated,
      unrestorableReason: file.unrestorable_reason ?? null,
      originalHash: file.original_hash,
      currentHash: file.current_hash,
    })),
  }
}
