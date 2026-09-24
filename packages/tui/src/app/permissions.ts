import type { PickerItem } from "../components"
import { PickerController } from "../picker-controller"
import { ProjectionRequestBroker, type ProjectionKind } from "../projection-requests"
import {
  type ApprovalDecision,
  type PermissionApprovalScope,
  type PermissionDecision,
  type PermissionModeDescriptor,
  type PermissionRuleDescriptor,
} from "../protocol"
import { type RottweilerState, type ToolProjection } from "../state"
import { commandPreview } from "../render"
import { permissionActionLabel, permissionModeLabel, permissionPatternLabel, permissionRuleActionLabel } from "../ui-presentation"

type PermissionPickerAction =
  | { readonly kind: "mode"; readonly mode: PermissionMode }
  | { readonly kind: "remove"; readonly ruleId: string }
  | { readonly kind: "revoke"; readonly approvalId: string; readonly scope: PermissionApprovalScope }
  | { readonly kind: "info" }
  | { readonly kind: "trust" }
type PermissionMode = PermissionModeDescriptor | "default"
interface PermissionModeChoice {
  readonly mode: PermissionMode
  readonly description: string
}
const PERMISSION_MODE_CHOICES: readonly PermissionModeChoice[] = [
  { mode: "strict", description: "Ask before edits and commands outside the safe list" },
  { mode: "auto-safe", description: "Allow safe actions and workspace edits · ask for the rest" },
  { mode: "yolo", description: "Skip approval prompts · explicit denials still apply" },
  { mode: "default", description: "Follow the launch policy" },
]
interface PermissionUiHost {
  readonly state: RottweilerState
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Readonly<Partial<Record<ProjectionKind, string>>>
  closePicker(): void
  submitPaletteCommand(content: string): void
  approve(tool: ToolProjection, decision: ApprovalDecision): void
}

/**
 * Permissions: approval policy, the rules that apply to this session, and
 * folder trust. Session rules and remembered approvals are removed in place;
 * configuration rules are read-only and say where they come from.
 */
export class PermissionUiController {
  readonly #host: PermissionUiHost
  #review: ToolProjection | null = null
  #newRule = false
  constructor(host: PermissionUiHost) { this.#host = host }
  openPermissionPicker(): void {
    this.#review = null
    this.#newRule = false
    this.#host.pickerController.begin("permissions")
    this.#host.requests.command({ type: "list_permissions" })
    this.#host.pickerController.refresh()
  }

  /**
   * "Always allow" for a waiting approval: choose between remembering this
   * exact invocation for the project and a reviewed project pattern. Both
   * choices also allow the waiting invocation; Esc returns to the prompt.
   */
  openAlwaysAllowReview(tool: ToolProjection): void {
    this.#review = tool
    this.#newRule = false
    this.#host.pickerController.begin("permissions")
    this.#host.pickerController.refresh()
  }

  #renderAlwaysAllowReview(): void {
    const tool = this.#review!
    const exact = exactInvocationLabel(tool)
    const pattern = suggestedPermissionPattern(tool)
    const allowNow = (decision: ApprovalDecision) => {
      this.#review = null
      this.#host.closePicker()
      this.#host.approve(tool, decision)
    }
    type Choice = "exact" | "pattern"
    const items: PickerItem<Choice>[] = [
      ...(tool.diff === null ? [{
        id: "always.exact", label: `Only ${exact}`, hint: "this project", primary: "allow",
        description: "Remembered for this project · runs now",
        detail: [
          exact,
          "",
          "Allows exactly this invocation in this project, now and in later sessions.",
          "Any change to the command, its arguments, or the workspace asks again.",
        ].join("\n"),
        value: "exact" as const,
      }] : []),
      {
        id: "always.pattern", label: `Anything matching ${pattern}…`, hint: "this project", primary: "review",
        description: "Review the pattern before saving · runs now",
        detail: [
          pattern,
          "",
          "Allows future calls that match this pattern in this project, now and in later sessions. You can edit it before saving, and remove it any time in /permissions.",
          "Every part of a compound shell command must match on its own. Network access, unsandboxed execution, and deny rules are still checked separately.",
        ].join("\n"),
        value: "pattern" as const,
      },
    ]
    this.#host.pickerController.show("ALWAYS ALLOW", items, (item) => {
      if (item.value === "exact") { allowNow("allow_project"); return }
      this.#openPermissionPatternPrompt("allow", () => this.#host.pickerController.refresh(), {
        initial: pattern,
        onSaved: () => allowNow("allow_once"),
      })
    }, {
      view: "review",
      emptyCopy: "",
      back: () => { this.#review = null; this.#host.closePicker() },
    })
  }

  openPermissionModePicker(): void {
    this.#host.pickerController.begin("permissionMode")
    this.#host.pickerController.refresh()
  }

  openTrustPicker(): void {
    this.#host.pickerController.begin("trust")
    this.#host.pickerController.refresh()
  }

  #openPermissionPatternPrompt(
    action: PermissionDecision,
    back: () => void,
    review?: { readonly initial: string; readonly onSaved: () => void },
  ): void {
    this.#host.pickerController.kind = "permissionInput"
    const scope = this.#host.pickerController.interaction
    this.#host.pickerController.openTextPrompt({
      title: `PERMISSIONS › ${permissionRuleActionLabel(action)}`,
      placeholder: "tool(glob), e.g. bash(cargo test*)",
      detail: "Saved for this project in your private Rottweiler data, never in the repository. Name a tool and an argument glob, e.g. bash(cargo test*) or edit(src/**). Remove it any time in /permissions.",
      ...(review === undefined ? {} : { initial: review.initial }),
      onSubmit: (pattern) => {
        if (!scope?.active) return
        this.#host.requests.command({ type: "add_permission_rule", scope: "project", pattern, action })
        if (review === undefined) this.#host.closePicker()
        else review.onSaved()
      },
      maxBytes: 2048,
      empty: "reject",
    }, () => {
      this.#host.pickerController.kind = "permissions"
      back()
    })
  }

  #permissionModeItems(): PickerItem<Extract<PermissionPickerAction, { kind: "mode" }>>[] {
    const current = this.#host.state.permissions?.runtime_mode ?? "default"
    return PERMISSION_MODE_CHOICES.map((choice) => ({
      id: `permissions.mode.${choice.mode}`,
      label: permissionModeLabel(choice.mode),
      ...(choice.mode === current ? { marker: "●", hint: "current" } : {}),
      ...(choice.mode === "yolo" ? { tone: "warning" as const } : {}),
      description: choice.description,
      primary: "switch",
      value: { kind: "mode", mode: choice.mode },
    }))
  }

  #selectPermissionMode(mode: PermissionMode, back: (() => void) | null): void {
    if (mode === "yolo") {
      this.#yoloBack = back
      this.#host.pickerController.anchored = false
      this.#host.pickerController.query = ""
      this.#host.pickerController.kind = "permissionYoloConfirm"
      this.#host.pickerController.refresh()
      return
    }
    this.#host.submitPaletteCommand(`/permissions mode ${mode}`)
  }
  #yoloBack: (() => void) | null = null

  render(kind: "permissionInput" | "trust" | "permissionMode" | "permissionYoloConfirm" | "permissions"): void {
    switch (kind) {
      case "permissionInput":
        break
      case "trust":
        this.#host.pickerController.show(
          "PERMISSIONS › Folder trust",
          [
            { id: "trust.status", label: "Show trust status", description: "Display this folder's current trust state", value: "/permissions trust status" },
            { id: "trust.grant", label: "Trust this folder", description: "Allow this workspace's executable project configuration", value: "/permissions trust grant" },
            { id: "trust.revoke", label: "Revoke trust", tone: "warning", description: "Disable this workspace's executable project configuration", value: "/permissions trust revoke" },
          ],
          (item) => this.#host.submitPaletteCommand(item.value),
          { primary: "run", back: () => this.openPermissionPicker() },
        )
        break
      case "permissionMode":
        this.#host.pickerController.show(
          "APPROVALS",
          this.#permissionModeItems(),
          (item) => this.#selectPermissionMode(item.value.mode, () => this.openPermissionModePicker()),
          { selectedId: `permissions.mode.${this.#host.state.permissions?.runtime_mode ?? "default"}` },
        )
        break
      case "permissionYoloConfirm": {
        const back = this.#yoloBack
        this.#host.pickerController.show(
          "APPROVALS › Turn approvals off?",
          [
            { id: "permissions.yolo.cancel", label: "Keep the current policy", description: "Nothing changes", value: false },
            { id: "permissions.yolo.confirm", label: "Turn approvals off", tone: "warning",
              description: "Every tool runs without asking · explicit denials still apply", value: true },
          ],
          (item) => {
            if (item.value) this.#host.submitPaletteCommand("/permissions mode yolo")
            else if (back !== null) back()
            else this.#host.closePicker()
          },
          { primary: "choose", ...(back === null ? {} : { back }) },
        )
        break
      }
      case "permissions":
        if (this.#review !== null) { this.#renderAlwaysAllowReview(); break }
        if (this.#newRule) { this.#renderNewRule(); break }
        this.#renderPermissions()
        break
    }
  }

  #renderNewRule(): void {
    const back = () => {
      this.#newRule = false
      this.#host.pickerController.refresh()
    }
    this.#host.pickerController.show("PERMISSIONS › New rule", (["allow", "ask", "deny"] as const).map(action => ({
      id: `permissions.add.${action}`,
      label: permissionRuleActionLabel(action),
      hint: permissionActionLabel(action).toLocaleLowerCase(),
      description: "Applies to this session · choose a tool or command pattern next",
      primary: "next",
      value: action,
    })), item => this.#openPermissionPatternPrompt(item.value, () => this.#host.pickerController.refresh()), { view: "new", back })
  }

  #renderPermissions(): void {
    const permissions = this.#host.state.permissions
    const permissionError = this.#host.projectionErrors.permissions
    if (permissions === null && permissionError === undefined) {
      this.#host.pickerController.showLoading("PERMISSIONS   /permissions", "Loading permission rules")
      return
    }
    if (permissions === null) {
      this.#host.pickerController.showStatus("PERMISSIONS   /permissions", "Permission rules could not be loaded", "Close and reopen this screen to retry.")
      return
    }
    const section = (id: string, label: string): PickerItem<PermissionPickerAction> =>
      ({ id: `permissions.section.${id}`, label, description: "", sectionHeader: true, value: { kind: "info" } })
    const readOnly = (id: string, rule: { action: PermissionDecision; pattern: string }, source: string): PickerItem<PermissionPickerAction> => ({
      id, label: permissionPatternLabel(rule.pattern), hint: permissionActionLabel(rule.action).toLocaleLowerCase(),
      description: `${source} · read-only`, detail: `${rule.pattern}\n\n${permissionActionLabel(rule.action)} · ${source}\nEdit the configuration file to change this rule.`,
      primary: null, value: { kind: "info" },
    })
    const removableRule = (rule: PermissionRuleDescriptor, scope: "project" | "session") => ({
      id: `permissions.remove.${rule.id}`, label: permissionPatternLabel(rule.pattern),
      hint: permissionActionLabel(rule.action).toLocaleLowerCase(),
      description: `${scope === "project" ? "This project" : "This session"} · ctrl+d removes it`,
      detail: `${rule.pattern}\n\n${permissionActionLabel(rule.action)} · ${scope === "project" ? "saved for this project" : "this session only"}`,
      primary: null, value: { kind: "remove", ruleId: rule.id } as const,
    })
    const projectRules = permissions.project_rules.map(rule => removableRule(rule, "project"))
    const sessionRules = permissions.session_rules.map(rule => removableRule(rule, "session"))
    const approvals = permissions.approvals.map((approval) => ({
      id: `permissions.revoke.${approval.id}`, label: approval.tool_name,
      hint: approval.scope === "project" ? "remembered · project" : "remembered · session",
      description: `${approval.scope === "project" ? "This project" : "This session"} · ctrl+d revokes it`,
      primary: null, value: { kind: "revoke", approvalId: approval.id, scope: approval.scope } as const,
    }))
    const projectApprovals = approvals.filter(item => item.value.scope === "project")
    const sessionApprovals = approvals.filter(item => item.value.scope === "session")
    const configured = permissions.effective_rules.map(rule => readOnly(`permissions.effective.${rule.id}`, rule, "trusted configuration"))
    const items: PickerItem<PermissionPickerAction>[] = [
      section("policy", "Approval policy"),
      ...this.#permissionModeItems(),
      ...(projectRules.length + projectApprovals.length === 0 ? [] : [section("project", "This project")]),
      ...projectRules,
      ...projectApprovals,
      ...(sessionRules.length + sessionApprovals.length === 0 ? [] : [section("session", "This session")]),
      ...sessionRules,
      ...sessionApprovals,
      ...(configured.length === 0 ? [] : [section("configured", "Configured rules")]),
      ...configured,
      section("workspace", "Workspace"),
      { id: "permissions.default", label: "Default for other tools", hint: permissionActionLabel(permissions.default).toLocaleLowerCase(),
        description: "Applies when no rule matches", primary: null, value: { kind: "info" } },
      { id: "permissions.trust", label: "Folder trust", description: "Show, grant, or revoke trust for this workspace's project configuration",
        primary: "open", value: { kind: "trust" } },
    ]
    const removable = (item: PickerItem<PermissionPickerAction> | null) =>
      item?.value.kind === "remove" || item?.value.kind === "revoke"
    this.#host.pickerController.show("PERMISSIONS   /permissions", items, (item) => {
      const action = item.value
      if (action.kind === "trust") this.openTrustPicker()
      else if (action.kind === "mode") this.#selectPermissionMode(action.mode, () => this.openPermissionPicker())
    }, {
      selectedId: `permissions.mode.${permissions.runtime_mode ?? "default"}`,
      notice: permissions.truncated === true ? { message: "inventory truncated", tone: "warning" } : null,
      keys: [
        { stroke: "ctrl+n", label: "new rule", run: () => {
          this.#newRule = true
          this.#host.pickerController.refresh()
        } },
        { stroke: "ctrl+d", label: "remove", available: removable, run: item => {
          const action = item?.value
          if (action?.kind === "remove") {
            this.#host.requests.command({ type: "remove_permission_rule", ruleId: action.ruleId })
          } else if (action?.kind === "revoke") {
            this.#host.requests.command({ type: "revoke_permission_approval", approvalId: action.approvalId, scope: action.scope })
          }
        } },
      ],
    })
  }
}

/** The waiting invocation as the user would recognize it. */
function exactInvocationLabel(tool: ToolProjection): string {
  const args = toolArguments(tool)
  if (tool.name === "bash" && typeof args.command === "string") return `\`${commandPreview(args.command).split("\n")[0]}\``
  const primary = primaryArgument(args)
  return primary === undefined ? `this ${tool.name} call` : `${tool.name} ${primary}`
}

/**
 * A reviewable starting pattern: the command's program for shell commands,
 * the containing directory for paths, and the origin for URLs. It is only a
 * suggestion; nothing is saved until the user submits it.
 */
export function suggestedPermissionPattern(tool: ToolProjection): string {
  const args = toolArguments(tool)
  if (tool.name === "bash" && typeof args.command === "string") {
    const program = args.command.trim().split(/\s+/, 1)[0] ?? ""
    const name = program.split("/").pop() ?? ""
    return /^[A-Za-z0-9_.+-]+$/.test(name) ? `bash(${name} *)` : `bash(${args.command.trim()})`
  }
  if (typeof args.url === "string") {
    try { return `${tool.name}(${new URL(args.url).origin}/**)` } catch { return `${tool.name}(${args.url})` }
  }
  const primary = primaryArgument(args)
  if (primary === undefined) return `${tool.name}(*)`
  const directory = primary.includes("/") ? primary.slice(0, primary.lastIndexOf("/")) : ""
  return directory === "" ? `${tool.name}(${primary})` : `${tool.name}(${directory}/**)`
}

function toolArguments(tool: ToolProjection): Record<string, unknown> {
  return tool.args !== null && typeof tool.args === "object" && !Array.isArray(tool.args)
    ? tool.args as Record<string, unknown>
    : {}
}

function primaryArgument(args: Record<string, unknown>): string | undefined {
  return ["path", "file_path", "filePath", "pattern", "query"]
    .map((key) => args[key])
    .find((value): value is string => typeof value === "string" && value.trim() !== "")
    ?.trim()
}
