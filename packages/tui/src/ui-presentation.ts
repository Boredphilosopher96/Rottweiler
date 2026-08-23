import { truncateToCells } from "./render/text"
import type { RottweilerState } from "./state"

type ProviderProjection = RottweilerState["providers"][number]

export function boundedUiText(value: string, maximum: number): string {
  const safe = value
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "")
    .replace(/\s+/g, " ")
    .trim()
  return truncateToCells(safe, maximum)
}

export function queuedMessageLabel(content: string): string {
  const firstLine = content.split(/\r?\n/, 1)[0] ?? ""
  const label = boundedUiText(firstLine, 64)
  return label.length === 0 ? "(empty message)" : label
}

export function timelineTurnLabel(content: string): string {
  const firstLine = content.split(/\r?\n/, 1)[0] ?? ""
  const label = boundedUiText(firstLine, 64)
  return label.length === 0 ? "(attachment-only message)" : label
}

export function modeDisplayName(id: string): string {
  const words = boundedUiText(id.replaceAll(/[-_]+/g, " "), 64)
  return words.length === 0 ? "Mode" : words[0]!.toUpperCase() + words.slice(1)
}

export type ModePickerValue =
  | { readonly kind: "mode"; readonly id: string }
  | { readonly kind: "retry" }

export interface ModePickerPresentation {
  readonly title: string
  readonly items: readonly {
    readonly id: string
    readonly label: string
    readonly description: string
    readonly value: ModePickerValue
  }[]
}

export function modePickerPresentation(
  state: Pick<RottweilerState, "mode" | "modes" | "modesTruncated">,
  error: string | undefined,
  loading: boolean,
): ModePickerPresentation {
  const status = loading
    ? " · refreshing"
    : error !== undefined
      ? " · load failed"
      : state.modesTruncated
        ? " · partial catalog"
        : ""
  const items: ModePickerPresentation["items"][number][] = state.modes.map((mode) => ({
    id: `mode:${mode.id}`,
    label: `${mode.id === state.mode ? "● " : ""}${modeDisplayName(mode.id)}`,
    description: boundedUiText(mode.description, 160),
    value: { kind: "mode", id: mode.id },
  }))
  if (error !== undefined) {
    items.unshift({
      id: "modes.retry",
      label: "Retry loading modes",
      description: boundedUiText(error, 160),
      value: { kind: "retry" },
    })
  }
  return { title: `Modes${status}`, items }
}

export function nextModeId(
  current: string | null,
  modes: readonly RottweilerState["modes"][number][],
): string {
  if (modes.length === 0) return current ?? "execute"
  const index = modes.findIndex((mode) => mode.id === current)
  return modes[index < 0 ? 0 : (index + 1) % modes.length]!.id
}

export function providerDisplayName(
  provider: Pick<ProviderProjection, "name" | "authKind">,
): string {
  return providerName(provider.name)
}

export function providerName(name: string): string {
  if (name === "openai_codex") return "OpenAI · ChatGPT"
  if (name === "openai") return "OpenAI API"
  if (name === "github_copilot") return "GitHub Copilot"
  if (name === "anthropic") return "Anthropic API"
  return name.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase())
}

export function mcpTransportLabel(transport: string): string {
  switch (transport) {
    case "http":
    case "streamable_http": return "Remote HTTPS"
    case "stdio": return "Local command"
    default: return "Connection"
  }
}

export function mcpStateLabel(state: string): string {
  switch (state) {
    case "disabled": return "Disabled"
    case "connecting": return "Connecting"
    case "ready": return "Connected"
    case "approval_required": return "Approval needed"
    case "failed": return "Connection failed"
    case "stopping": return "Stopping"
    default: return "Unavailable"
  }
}

export function providerConnectionStatus(provider: ProviderProjection): string {
  if (provider.authenticated && provider.reachable) return "Connected"
  if (provider.authenticated) return "Signed in · models unavailable"
  if (!provider.configured) return "Not set up"
  switch (provider.authKind) {
    case "oauth": return provider.name === "openai_codex" ? "Sign in with ChatGPT" : "Sign in required"
    case "device_flow": return "Sign in with GitHub"
    case "api_key": return "API key required"
    case "none": return "Unavailable"
  }
}

export function providerStatusDetail(provider: ProviderProjection): string {
  if (provider.authenticated && provider.reachable) return ""
  if (provider.authenticated && !provider.reachable) {
    const status = provider.status?.toLowerCase() ?? ""
    if (status.includes("auth")) return "GitHub rejected this sign-in · sign in again"
    if (status.includes("rate limit")) return "Model catalog is rate limited · retry shortly"
    if (status.includes("timed out") || status.includes("network") || status.includes("server")) {
      return "Couldn't reach the model catalog · retry"
    }
    if (status.includes("invalid") || status.includes("unsupported")) {
      return "The provider returned an unusable model catalog"
    }
    return "Couldn't load available models · retry"
  }
  const status = provider.status?.toLowerCase() ?? ""
  if (status.includes("setup required") || status.includes("not configured")) {
    return "Complete setup to continue"
  }
  if (status.includes("credential") || status.includes("auth")) return "Sign in again to continue"
  if (status.includes("model") || status.includes("discovery")) {
    return "Couldn't load available models"
  }
  return ""
}

export function contextPanelHasContent(state: RottweilerState): boolean {
  const statusPaths = state.workspaceStatus?.changedPaths
  const hasChangedFiles = statusPaths === undefined
    ? (state.review?.files.length ?? 0) > 0
    : statusPaths.length > 0
  return state.todos.length > 0 ||
    hasChangedFiles ||
    state.runtimeServices.some((service) => service.name.length > 0) ||
    state.mcpServers.some((server) =>
      server.state.type === "connecting" ||
      server.state.type === "ready" ||
      server.state.type === "stopping"
    )
}

export function modelAvailabilityLabel(model: RottweilerState["models"][number]): string {
  if (model.available !== false) return "available"
  const status = model.status?.toLowerCase() ?? ""
  if (status.includes("credential") || status.includes("auth")) return "sign in again"
  if (status.includes("discovery") || status.includes("catalog")) {
    return "couldn't verify availability"
  }
  return "unavailable"
}

export function modelAliasDescription(
  alias: RottweilerState["modelAliases"][number],
  models: readonly RottweilerState["models"][number][],
): string {
  const candidates = alias.candidates.map((candidate) => boundedUiText(candidate, 64))
  const candidateModels = alias.candidates.map((candidate) =>
    models.find((model) => model.id === candidate),
  )
  const availability =
    candidateModels.length > 0 && candidateModels.every((model) => model !== undefined)
      ? candidateModels.every((model) => model?.available === false)
        ? "no available route"
        : "available"
      : ""
  return boundedUiText(
    ["failover", candidates.join(" → "), availability].filter(Boolean).join(" · "),
    160,
  )
}

export function permissionActionLabel(action: "allow" | "ask" | "deny"): string {
  switch (action) {
    case "allow": return "Allowed automatically"
    case "ask": return "Ask first"
    case "deny": return "Not allowed"
  }
}

export function permissionRuleActionLabel(action: "allow" | "ask" | "deny"): string {
  switch (action) {
    case "allow": return "Always allow matching tools"
    case "ask": return "Ask before matching tools run"
    case "deny": return "Never allow matching tools"
  }
}

export function permissionPatternLabel(pattern: string): string {
  const callPattern = /^([^()]+)\((.*)\)$/.exec(pattern)
  if (callPattern === null) return pattern.replaceAll("_", " ")
  const tool = callPattern[1] ?? pattern
  const argumentPattern = callPattern[2] ?? ""
  if (argumentPattern.length === 0 || argumentPattern === "*") return `${tool} · any arguments`
  return `${tool} · arguments matching ${argumentPattern}`
}
