import {
  FuzzyPickerRenderable,
  type PickerItem
} from "../components"
import { PickerController } from "../picker-controller"
import {
  ProjectionRequestBroker,
  type ProjectionKind,
} from "../projection-requests"
import {
  type PermissionApprovalScope,
  type PermissionDecision,
  type PermissionModeDescriptor
} from "../protocol"
import {
  type RottweilerState
} from "../state"
import {
  permissionActionLabel,
  permissionPatternLabel,
  permissionRuleActionLabel
} from "../ui-presentation"

type PermissionPickerAction =
  | { readonly kind: "refresh" }
  | { readonly kind: "mode"; readonly mode: PermissionMode }
  | { readonly kind: "add"; readonly action: PermissionDecision }
  | { readonly kind: "remove"; readonly ruleId: string }
  | { readonly kind: "revoke"; readonly approvalId: string; readonly scope: PermissionApprovalScope }
  | { readonly kind: "info" }
type PermissionModePickerAction = Extract<PermissionPickerAction, { readonly kind: "mode" }>
type PermissionMode = PermissionModeDescriptor | "default"
interface PermissionModeChoice {
  readonly mode: PermissionMode
  readonly description: string
}
const PERMISSION_MODE_CHOICES: readonly PermissionModeChoice[] = [
  { mode: "strict", description: "Ask before every tool use" },
  { mode: "auto-safe", description: "Ask only for risky actions" },
  { mode: "yolo", description: "Never ask · dangerous" },
  { mode: "default", description: "Follow the launch policy" },
]
interface PermissionUiHost {
  readonly state: RottweilerState
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Readonly<Partial<Record<ProjectionKind, string>>>
  closePicker(): void
  submitPaletteCommand(content: string): void
}
export class PermissionUiController {
  #promptScope = {}
  readonly #host: PermissionUiHost
  constructor(host: PermissionUiHost) { this.#host = host }
  pickerClosed(): void { this.#promptScope = {} }
  openPermissionPicker(): void {
    this.#host.pickerController.begin("permissions")
    this.#host.requests.command({ type: "list_permissions" })
    this.#host.pickerController.refresh()
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
    action: PermissionDecision
  ): void {
    this.#host.pickerController.kind = "permissionInput"
    const scope = this.#promptScope = {}
    this.#host.picker.openTextPrompt({
      title: `Add ${action} permission rule`, placeholder: "tool(glob), e.g. bash(cargo test*)", onSubmit: (pattern) => {
        if (scope !== this.#promptScope) return
        this.#host.closePicker()
        this.#host.requests.command({ type: "add_session_permission_rule", pattern, action })
      }, maxBytes: 2048, empty: "reject"
    })
  }

  #permissionModeItems(): PickerItem<PermissionModePickerAction>[] {
    const current = this.#host.state.permissions?.runtime_mode ?? "default"
    return PERMISSION_MODE_CHOICES.map((choice) => ({
      id: `permissions.mode.${choice.mode}`,
      label: choice.mode === current ? `● ${choice.mode}` : choice.mode,
      description: choice.description,
      value: { kind: "mode", mode: choice.mode },
    }))
  }

  #selectPermissionMode(mode: PermissionMode): void {
    if (mode === "yolo") {
      this.#host.pickerController.anchored = false
      this.#host.pickerController.query = ""
      this.#host.pickerController.kind = "permissionYoloConfirm"
      this.#host.pickerController.refresh()
      return
    }
    this.#host.submitPaletteCommand(`/permissions mode ${mode}`)
  }
  render(kind: "permissionInput" | "trust" | "permissionMode" | "permissionYoloConfirm" | "permissions"): void {
    switch (kind) {
      case "permissionInput":
        break
      case "trust":
        this.#host.pickerController.show(
          "Folder trust",
          [
            {
              id: "trust.status",
              label: "Show trust status",
              description: "Display the current folder trust state",
              value: "/trust status",
            },
            {
              id: "trust.grant",
              label: "Grant trust",
              description: "Allow executable project configuration",
              value: "/trust grant",
            },
            {
              id: "trust.revoke",
              label: "Revoke trust",
              description: "Disable executable project configuration",
              value: "/trust revoke",
            },
          ],
          (item) => this.#host.submitPaletteCommand(item.value),
        )
        break
      case "permissionMode":
        this.#host.pickerController.show(
          "Permission mode",
          this.#permissionModeItems(),
          (item) => this.#selectPermissionMode(item.value.mode),
        )
        break
      case "permissionYoloConfirm":
        this.#host.pickerController.show(
          "Run every tool without asking?",
          [
            {
              id: "permissions.yolo.confirm",
              label: "Yes, enable yolo",
              description: "Never ask before tool use",
              value: true,
            },
            {
              id: "permissions.yolo.cancel",
              label: "Cancel",
              description: "Keep the current permission mode",
              value: false,
            },
          ],
          (item) => {
            if (item.value) this.#host.submitPaletteCommand("/permissions mode yolo")
            else this.#host.closePicker()
          },
        )
        break
      case "permissions":
        {
          const permissions = this.#host.state.permissions
          const permissionError = this.#host.projectionErrors.permissions
          if (permissions === null && permissionError === undefined) {
            this.#host.pickerController.showLoading("Permission rules", "Loading permission rules")
            break
          }
          if (permissions === null) {
            this.#host.pickerController.showStatus(
              "Permission rules",
              "Permission rules could not be loaded",
              "Close and reopen this panel to retry.",
            )
            break
          }
          const items: PickerItem<PermissionPickerAction>[] = [
            ...this.#permissionModeItems(),
            {
              id: "permissions.refresh",
              label: `Default behavior · ${permissionActionLabel(permissions.default)}`,
              description: permissions.truncated === true
                ? "Inventory truncated · refresh after removing entries"
                : "Refresh effective rules and remembered approvals",
              value: { kind: "refresh" },
            },
            ...(["allow", "ask", "deny"] as const).map((action) => ({
              id: `permissions.add.${action}`,
              label: permissionRuleActionLabel(action),
              description: "Applies to this session · choose a tool or command pattern",
              value: { kind: "add", action } as const,
            })),
            ...permissions.effective_rules.map((rule) => ({
              id: `permissions.effective.${rule.id}`,
              label: `${permissionActionLabel(rule.action)} · ${permissionPatternLabel(rule.pattern)}`,
              description: "Trusted configuration · read-only",
              value: { kind: "info" } as const,
            })),
            ...permissions.project_rules.map((rule) => ({
              id: `permissions.project.${rule.id}`,
              label: `${permissionActionLabel(rule.action)} · ${permissionPatternLabel(rule.pattern)}`,
              description: "Project rule · read-only",
              value: { kind: "info" } as const,
            })),
            ...permissions.session_rules.map((rule) => ({
              id: `permissions.remove.${rule.id}`,
              label: `Remove · ${permissionPatternLabel(rule.pattern)}`,
              description: `This session · ${permissionActionLabel(rule.action).toLowerCase()} · select to remove`,
              value: { kind: "remove", ruleId: rule.id } as const,
            })),
            ...permissions.approvals.map((approval) => ({
              id: `permissions.revoke.${approval.id}`,
              label: `Revoke · ${approval.tool_name}`,
              description: `${approval.scope === "project" ? "This project" : "This session"} · remembered approval`,
              value: {
                kind: "revoke",
                approvalId: approval.id,
                scope: approval.scope,
              } as const,
            })),
          ]
          this.#host.pickerController.show(
            "Permission rules",
            items,
            (item) => {
              const action = item.value
              if (action.kind === "refresh") this.#host.requests.command({ type: "list_permissions" })
              else if (action.kind === "mode") {
                this.#selectPermissionMode(action.mode)
              }
              else if (action.kind === "add") this.#openPermissionPatternPrompt(action.action)
              else if (action.kind === "remove") {
                this.#host.requests.command({ type: "remove_session_permission_rule", ruleId: action.ruleId })
              } else if (action.kind === "revoke") {
                this.#host.requests.command({
                  type: "revoke_permission_approval",
                  approvalId: action.approvalId,
                  scope: action.scope,
                })
              }
            }
          )
        }
        break
    }
  }
}
