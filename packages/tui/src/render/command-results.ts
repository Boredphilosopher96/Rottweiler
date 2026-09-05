import {
  type BoundedCommandTextProjection,
  type CommandResultProjection,
  type StructuredCommandResultRow
} from "./command-types"

const HIDDEN_COMMAND_RESULT_FIELDS = new Set([
  "protocol_version",
  "request_id",
  "session_id",
  "turn_id",
  "item_id",
  "stable_prefix_hash",
  "machine_local_path",
  "original_hash",
  "current_hash",
  "base_hash",
  "diff_hash",
  "truncated",
])

/** Project command results without retaining renderer-specific Markdown. */
export function projectCommandResult(
  name: string,
  source: string,
): CommandResultProjection {
  if (source.length > 8192) return { kind: "unsafe_structured" }
  const trimmed = source.trim()
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      const parsed: unknown = JSON.parse(trimmed)
      const rows = projectStructuredRows(parsed, 0)
      return {
        kind: "structured",
        rows: rows.slice(0, 24),
        omittedRowCount: Math.max(0, rows.length - 24),
      }
    } catch {
      // A structured-looking result that cannot be decoded is not safe UI state.
      // It may be a truncated wire payload, so fail closed instead of retaining it.
      return { kind: "unsafe_structured" }
    }
  }
  if (name === "help") return projectHelpCommand(trimmed)
  if (name === "status") return projectStatusCommand(trimmed)
  if (name === "mode") return projectModeCommand(trimmed)
  if (name === "permissions") return projectPermissionCommand(trimmed)
  if (name === "plan") return projectPlanCommand(trimmed)
  if (name === "review") return projectReviewCommand(trimmed)
  if (name === "trust") return projectTrustCommand(trimmed)
  if (name === "mcp") return projectMcpCommand(trimmed)
  const completion = commandCompletionTitle(name)
  if (completion !== null) {
    return {
      kind: "completion",
      title: completion,
      detail: trimmed.length === 0 ? null : singleLineCommand(trimmed, 180),
    }
  }
  return { kind: "message", content: projectBoundedText(trimmed.split("\n"), 32) }
}

function projectHelpCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "help" }> {
  const commands = source.split("\n").map((line) => line.trim()).filter(Boolean).flatMap((line) => {
    const [usage, description] = line.split(/\s+—\s+/, 2)
    return usage === undefined || description === undefined ? [] : [{ usage, description }]
  })
  return {
    kind: "help",
    commands: commands.slice(0, 30),
    omittedCommandCount: Math.max(0, commands.length - 30),
    fallback: commands.length > 0 || source.length === 0
      ? null
      : projectUnboundedText(source),
  }
}

function projectStatusCommand(source: string): CommandResultProjection {
  const values = new Map(source.split("\n").flatMap((line) => {
    const separator = line.indexOf(":")
    return separator < 0 ? [] : [[line.slice(0, separator).trim().toLowerCase(), line.slice(separator + 1).trim()]]
  }))
  const agent = values.get("agent")
  const mode = values.get("mode")
  const queued = values.get("queued messages")
  if (agent === undefined || mode === undefined || queued === undefined) {
    return { kind: "message", content: projectUnboundedText(source) }
  }
  return { kind: "status", agent, mode, queuedMessages: queued }
}

function projectPermissionCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "permissions" }> {
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  if (lines.length <= 1) {
    return {
      kind: "permissions",
      summary: lines[0] ?? null,
      mode: null,
      defaultPermission: null,
      rememberedApprovals: null,
      rules: [],
      omittedRuleCount: 0,
    }
  }

  const mode = lines.find((line) => /^permission mode:/i.test(line))?.split(":", 2)[1]?.trim()
  const fallback = lines.find((line) => /^default permission:/i.test(line))?.split(":", 2)[1]?.trim()
  const approvals = lines.find((line) => /^remembered approvals:/i.test(line))
  const rules: Extract<CommandResultProjection, { readonly kind: "permissions" }>["rules"][number][] = []
  let scope: "Project" | "Session" = "Project"
  for (const line of lines) {
    if (/^configured rules:/i.test(line)) {
      scope = "Project"
      continue
    }
    if (/^session rules:/i.test(line)) {
      scope = "Session"
      continue
    }
    if (/^this session:/i.test(line)) {
      scope = "Session"
      continue
    }
    if (/^this project:/i.test(line)) {
      scope = "Project"
      continue
    }
    if (!line.startsWith("- ")) continue
    const values = line.slice(2).split(" · ")
    // Approval inventory rows contain an opaque revocation id. The dedicated
    // permission picker owns revocation; the transcript should show intent,
    // not internal credential/rule identifiers.
    if (values.length >= 3 && values.at(-1)?.startsWith("revoke with ")) {
      rules.push({ scope, decision: "remembered", target: values[0] ?? "tool", remembered: true })
      continue
    }
    rules.push({
      scope,
      decision: values[0] ?? "ask",
      target: values.slice(1).join(" · ") || "all tools",
      remembered: false,
    })
  }
  return {
    kind: "permissions",
    summary: null,
    mode: mode ?? null,
    defaultPermission: fallback ?? null,
    rememberedApprovals: approvals?.replace(/^remembered approvals:/i, "") ?? null,
    rules: rules.slice(0, 16),
    omittedRuleCount: Math.max(0, rules.length - 16),
  }
}

function projectModeCommand(source: string): CommandResultProjection {
  const match = /^(?:active mode:|mode changed to)\s*(\S+)/i.exec(source)
  if (match === null) return source.length === 0
    ? { kind: "mode", mode: null, active: false }
    : { kind: "message", content: projectBoundedText(source.split("\n"), 32) }
  return {
    kind: "mode",
    mode: match[1] ?? "execute",
    active: source.toLocaleLowerCase().startsWith("active"),
  }
}

function projectPlanCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "plan" }> {
  if (source.length === 0 || /^no plan/i.test(source)) {
    return { kind: "plan", title: null, body: null }
  }
  const lines = source.split("\n")
  const title = lines.shift()?.trim() ?? "Plan"
  return { kind: "plan", title, body: projectBoundedText(lines, 32) }
}

function projectReviewCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "review" }> {
  if (source.length === 0 || /no changed files/i.test(source)) {
    return { kind: "review", summary: null, files: [], omittedFileCount: 0 }
  }
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  const summary = lines.shift() ?? "Session review"
  const files = lines.filter((line) => line.startsWith("- ")).map((line) => {
    const [path, status, note] = line.slice(2).split(" · ")
    return { path: path ?? "file", status: status ?? "changed", note: note ?? "" }
  })
  return {
    kind: "review",
    summary,
    files: files.slice(0, 20),
    omittedFileCount: Math.max(0, files.length - 20),
  }
}

function projectTrustCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "trust" }> {
  if (source.length === 0) return { kind: "trust", trust: "updated", message: null }
  const safe = singleLineCommand(source, 200)
  const trusted = /(?:^|\b)(?:trusted|granted)(?:\b|$)/i.test(safe) && !/untrusted|not trusted/i.test(safe)
  const revoked = /revoked|untrusted|not trusted/i.test(safe)
  return {
    kind: "trust",
    trust: trusted ? "trusted" : revoked ? "untrusted" : "unknown",
    message: safe,
  }
}

function projectMcpCommand(
  source: string,
): Extract<CommandResultProjection, { readonly kind: "mcp" }> {
  if (source.length === 0) {
    return { kind: "mcp", updated: true, servers: [], omittedServerCount: 0, fallback: null }
  }
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  const servers = lines.flatMap((line) => {
    const values = line.replace(/^-\s*/, "").split(" · ")
    return values.length < 2
      ? []
      : [{ name: values[0] ?? "Server", status: values.slice(1).join(" · ") }]
  })
  return {
    kind: "mcp",
    updated: false,
    servers: servers.slice(0, 20),
    omittedServerCount: Math.max(0, servers.length - 20),
    fallback: servers.length === 0 ? projectBoundedText(source.split("\n"), 32) : null,
  }
}

function commandCompletionTitle(name: string): string | null {
  return ({
    compact: "Compaction started",
    interrupt: "Interrupt requested",
    rewind: "Session rewound",
    fork: "Session forked",
    "add-dir": "Workspace updated",
    init: "Workspace initialized",
    "deep-init": "Workspace initialized",
  } as Record<string, string>)[name] ?? null
}

function projectBoundedText(
  lines: readonly string[],
  maximum: number,
): BoundedCommandTextProjection {
  return {
    lines: lines.slice(0, maximum),
    omittedLineCount: Math.max(0, lines.length - maximum),
  }
}

function projectUnboundedText(source: string): BoundedCommandTextProjection {
  return { lines: source.split("\n"), omittedLineCount: 0 }
}

function singleLineCommand(source: string, maximum: number): string {
  const safe = source.replace(/[\u0000-\u001f\u007f-\u009f]/g, " ").replace(/\s+/g, " ").trim()
  return safe.length <= maximum ? safe : `${safe.slice(0, maximum - 1)}…`
}

function projectStructuredRows(
  value: unknown,
  depth: number,
  label?: string,
): StructuredCommandResultRow[] {
  if (depth > 5) return label === undefined
    ? []
    : [{ prefixes: [], label, value: { kind: "details_omitted" } }]
  if (value === null || value === undefined) {
    return label === undefined ? [] : [{ prefixes: [], label, value: { kind: "none" } }]
  }
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return [{
      prefixes: [],
      label: label ?? null,
      value: typeof value === "string"
        ? { kind: "string", value }
        : typeof value === "number"
          ? { kind: "number", value }
          : { kind: "boolean", value },
    }]
  }
  if (Array.isArray(value)) {
    if (value.length === 0) {
      return [{ prefixes: [], label: label ?? null, value: { kind: "empty_list" } }]
    }
    const heading: StructuredCommandResultRow[] = label === undefined
      ? []
      : [{ prefixes: [], label, value: { kind: "heading" } }]
    return [
      ...heading,
      ...value.flatMap((item) =>
        projectStructuredRows(item, depth + 1).map((row, index) => ({
          ...row,
          prefixes: [index === 0 ? "bullet" as const : "indent" as const, ...row.prefixes],
        })),
      ),
    ]
  }
  if (typeof value !== "object") return []
  const record = value as Record<string, unknown>
  const entries = Object.entries(record).filter(
    ([key, item]) =>
      !HIDDEN_COMMAND_RESULT_FIELDS.has(key) &&
      !(key === "data" && item !== null && typeof item === "object"),
  )
  const unwrapped = record.data
  const rows =
    unwrapped !== null && typeof unwrapped === "object"
      ? projectStructuredRows(unwrapped, depth + 1)
      : entries.flatMap(([key, item]) => {
        return sensitiveCommandResultField(key)
          ? [{ prefixes: [], label: key, value: { kind: "redacted" as const } }]
          : projectStructuredRows(item, depth + 1, key)
      })
  return label === undefined || rows.length === 0
    ? rows
    : [
      { prefixes: [], label, value: { kind: "heading" } },
      ...rows.map((row) => ({ ...row, prefixes: ["indent" as const, ...row.prefixes] })),
    ]
}

function sensitiveCommandResultField(key: string): boolean {
  return /token|secret|password|authorization|api[_-]?key|credential/i.test(key)
}
