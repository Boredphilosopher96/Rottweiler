import { McpEnvironmentDraft, MCP_ENVIRONMENT_DRAFT_LIMITS } from "./mcp-draft"
import {
  FuzzyPickerRenderable,
  ListDetailRenderable,
  type TextPromptOptions,
  type PickerItem
} from "../components"
import {
  createMcpBrowserModel,
  type McpBrowserAction,
  type McpCatalog
} from "../mcp-browser"
import { PickerController } from "../picker-controller"
import {
  ProjectionRequestBroker,
  type ProjectionKind,
} from "../projection-requests"
import {
  MCP_SERVER_ID_PATTERN
} from "../protocol"
import {
  type RottweilerState
} from "../state"
import {
  mcpStateLabel,
  mcpTransportLabel
} from "../ui-presentation"


type McpServerAction =
  | { readonly kind: "toggle"; readonly server: string; readonly enabled: boolean }
  | { readonly kind: "review"; readonly server: string }
  | { readonly kind: "approve"; readonly server: string; readonly fingerprint: string }
  | { readonly kind: "remove"; readonly server: string }
interface McpUiHost {
  readonly state: RottweilerState
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly browser: ListDetailRenderable<McpBrowserAction>
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Readonly<Partial<Record<ProjectionKind, string>>>
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly statusHeight: number
  readonly composerDockHeight: number
  readonly vim: boolean
  closePicker(): void
  modalOpened(): void
  projectError(code: string, message: string, retryable?: boolean): void
}

export class McpUiController {
  readonly #host: McpUiHost
  #promptScope = {}
  constructor(host: McpUiHost) { this.#host = host }
  get hasDraft(): boolean { return this.#mcpDraftName !== null }
  pickerClosed(): void {
    this.#promptScope = {}
    this.#clearMcpDraft()
    this.#mcpActionName = null
  }

  #prompt(options: TextPromptOptions): void {
    const scope = this.#promptScope
    this.#host.picker.openTextPrompt({ ...options, onSubmit: value => {
      if (scope === this.#promptScope) options.onSubmit(value)
    } })
  }
  #mcpDraftName: string | null = null
  #mcpDraftExecutable: string | null = null
  #mcpDraftArgs: string[] = []
  readonly #environment = new McpEnvironmentDraft()
  #mcpActionName: string | null = null
  openMcpPicker(): void {
    if (this.#host.state.replay.active) return
    this.pickerClosed()
    this.#host.pickerController.begin("mcp")
    this.resize(
      this.#host.terminalWidth,
      this.#host.terminalHeight,
    )
    this.#host.requests.command({ type: "list_mcp_servers" })
    this.#host.pickerController.refresh()
    this.#host.browser.input.focus()
  }

  #activateMcpBrowserAction(action: McpBrowserAction): void {
    if (action.kind === "retry") {
      this.#host.requests.command({ type: "list_mcp_servers" })
      this.#host.pickerController.refresh()
      return
    }
    this.#host.browser.visible = false
    this.#host.browser.input.blur()
    if (action.kind === "addHttp") {
      this.#openMcpHttpNamePrompt()
    } else if (action.kind === "addStdio") {
      this.#openMcpStdioNamePrompt()
    } else {
      this.#openMcpActionPicker(action.server)
    }
  }

  #openMcpActionPicker(server: string): void {
    if (this.#host.state.replay.active) return
    this.#mcpActionName = server
    this.#host.pickerController.kind = "mcpActions"
    this.#host.pickerController.refresh()
  }

  #openMcpHttpNamePrompt(): void {
    if (this.#host.state.replay.active) return
    this.#host.pickerController.kind = "mcpInput"
    this.#prompt({
      title: "Add remote MCP server", placeholder: "server name", onSubmit: (name) => {
        if (!new RegExp(MCP_SERVER_ID_PATTERN).test(name)) {
          this.#host.projectError(
            "mcp_name_invalid",
            "MCP server name is invalid"
          )
          return
        }
        this.#mcpDraftName = name
        this.#prompt({
          title: "Remote MCP endpoint", placeholder: "https://example.com/mcp", onSubmit: (endpoint) => {
            const server = this.#mcpDraftName
            this.#mcpDraftName = null
            this.#host.closePicker()
            if (server === null) return
            let parsed: URL
            try {
              parsed = new URL(endpoint)
            } catch {
              this.#host.projectError(
                "mcp_endpoint_invalid",
                "MCP endpoint must be an absolute HTTPS URL"
              )
              return
            }
            if (
              parsed.protocol !== "https:" ||
              parsed.username !== "" ||
              parsed.password !== "" ||
              parsed.search !== "" ||
              parsed.hash !== ""
            ) {
              this.#host.projectError(
                "mcp_endpoint_invalid",
                "MCP endpoint must be HTTPS without credentials, query, or fragment"
              )
              return
            }
            this.#host.requests.command({ type: "add_mcp_http_server", name: server, endpoint })
            this.openMcpPicker()
          }, maxBytes: 2048, empty: "reject"
        })
      }, maxBytes: 2048, empty: "reject"
    })
  }

  #openMcpStdioNamePrompt(): void {
    if (this.#host.state.replay.active) return
    this.#clearMcpDraft()
    this.#host.pickerController.kind = "mcpInput"
    this.#prompt({
      title: "Server name, e.g. docs", placeholder: "docs", onSubmit: (name) => {
        if (!new RegExp(MCP_SERVER_ID_PATTERN).test(name)) {
          this.#host.projectError("mcp_name_invalid", "MCP server name is invalid")
          return
        }
        this.#mcpDraftName = name
        this.#prompt({
          title: "Executable path, e.g. /usr/local/bin/docs-mcp", placeholder: "/usr/local/bin/docs-mcp", onSubmit: (executable) => {
            this.#mcpDraftExecutable = executable
            this.#prompt({
              title: "Arguments separated by spaces · quoting is not supported · leave empty for none", placeholder: "--stdio", onSubmit: (submitted) => {
                const trimmed = submitted.trim()
                this.#mcpDraftArgs = trimmed.length === 0
                  ? []
                  : trimmed.split(/[\t\n\v\f\r ]+/)
                this.#openMcpEnvironmentPrompt()
              }, maxBytes: 64 * 1024, empty: "allow"
            })
          }, maxBytes: 16 * 1024, empty: "reject"
        })
      }, maxBytes: 96, empty: "reject"
    })
  }

  #openMcpEnvironmentPrompt(): void {
    if (this.#host.state.replay.active) {
      this.#host.closePicker()
      return
    }
    this.#prompt({
      title: "Environment variable as KEY=VALUE · leave empty to finish", placeholder: "", onSubmit: (submitted) => {
        const entry = submitted
        if (entry.length === 0) {
          const name = this.#mcpDraftName
          const executable = this.#mcpDraftExecutable
          const args = this.#mcpDraftArgs
          const environment = this.#environment.take()
          this.#host.closePicker()
          if (name === null || executable === null) return
          this.#host.requests.command({
            type: "add_mcp_stdio_server",
            name,
            executable,
            args,
            environment,
          })
          this.openMcpPicker()
          return
        }
        const separator = entry.indexOf("=")
        if (separator <= 0) {
          this.#host.projectError(
            "mcp_environment_invalid",
            "Environment variable must use KEY=VALUE with a non-empty key",
          )
          this.#openMcpEnvironmentPrompt()
          return
        }
        if (!this.#environment.set(entry.slice(0, separator), entry.slice(separator + 1))) {
          this.#host.projectError("mcp_environment_full", `Environment draft is full (${MCP_ENVIRONMENT_DRAFT_LIMITS.entries} entries / ${MCP_ENVIRONMENT_DRAFT_LIMITS.bytes / 1024} KiB). Leave the field empty to finish or press Esc to cancel.`)
        }
        this.#openMcpEnvironmentPrompt()
      }, maxBytes: 16 * 1024 + 129, empty: "allow"
    })
  }

  #clearMcpDraft(): void {
    this.#mcpDraftName = null
    this.#mcpDraftExecutable = null
    this.#mcpDraftArgs = []
    this.#environment.clear()
  }

  resize(width: number, height: number): void {
    const primaryHeight = Math.max(
      6,
      height - this.#host.statusHeight - this.#host.composerDockHeight,
    )
    this.#host.browser.resizeForTerminal(width, height, primaryHeight)
  }
  render(kind: "mcp" | "mcpInput" | "mcpActions" | "mcpRemoveConfirm"): void {
    switch (kind) {
      case "mcpInput":
        break
      case "mcp": {
        this.resize(
          this.#host.terminalWidth,
          this.#host.terminalHeight,
        )
        const catalog: McpCatalog = this.#host.projectionErrors.mcp === undefined
          ? this.#host.state.mcpServers.length === 0 && this.#host.requests.current("mcp") !== null
            ? { kind: "loading" }
            : { kind: "ready", servers: this.#host.state.mcpServers }
          : {
            kind: "error",
            message: this.#host.projectionErrors.mcp,
            stale: this.#host.state.mcpServers,
          }
        const query = this.#host.browser.visible
          ? this.#host.browser.input.value
          : this.#host.pickerController.query
        const preserveSelection = query === this.#host.pickerController.query
        this.#host.pickerController.query = query
        const model = createMcpBrowserModel({
          catalog,
          review: this.#host.state.mcpApprovalReview,
          query,
          selectedId: this.#host.browser.visible && preserveSelection
            ? this.#host.browser.selectedId
            : null,
        })
        const presentation = this.#host.vim && model.status.includes("Esc close")
          ? { ...model, status: model.status.replace("Esc close", "Esc×2 close") }
          : model
        if (this.#host.browser.visible) {
          this.#host.browser.refresh(presentation)
        } else {
          this.#host.picker.close()
          this.#host.browser.open(presentation, (action) => {
            this.#activateMcpBrowserAction(action)
          }, {
            onQuery: () => this.#host.pickerController.refresh(),
            onSelection: () => this.#host.pickerController.refresh(),
            onRetry: () => {
              this.#host.requests.command({ type: "list_mcp_servers" })
              this.#host.pickerController.refresh()
            },
          })
          this.#host.modalOpened()
        }
        break
      }
      case "mcpActions": {
        const server = this.#host.state.mcpServers.find(
          (candidate) => candidate.name === this.#mcpActionName,
        )
        if (server === undefined) {
          this.#mcpActionName = null
          this.#host.pickerController.kind = "mcp"
          this.#host.pickerController.refresh()
          break
        }
        const review = this.#host.state.mcpApprovalReview?.server === server.name
          ? this.#host.state.mcpApprovalReview
          : null
        const deferred = server.enabled && server.state.type === "disabled"
        const items: PickerItem<McpServerAction>[] = [
          {
            id: `mcp.toggle.${server.name}`,
            label: deferred || !server.enabled ? "Enable" : "Disable",
            description: `${mcpStateLabel(server.state.type)} · applies to this live session and persists after validation`,
            value: {
              kind: "toggle",
              server: server.name,
              enabled: deferred ? false : server.enabled,
            },
          },
          {
            id: `mcp.review.${server.name}`,
            label: "Review fingerprint",
            description: server.approved ? "Review the approved configuration identity" : "Review before approval",
            value: { kind: "review", server: server.name },
          },
          ...(review === null ? [] : [{
            id: `mcp.approve.${review.server}`,
            label: "Approve",
            description: `${mcpTransportLabel(review.transport)} · ${review.endpoint ?? "local process"} · configuration fingerprint ${review.fingerprint}`,
            value: { kind: "approve", server: review.server, fingerprint: review.fingerprint },
          }] satisfies PickerItem<McpServerAction>[]),
          {
            id: `mcp.remove.${server.name}`,
            label: "Remove",
            description: "Delete this server from the live session and user configuration",
            value: { kind: "remove", server: server.name },
          },
        ]
        this.#host.pickerController.show(
          `MCP actions · ${server.name}`,
          items,
          (item) => {
            const action = item.value
            if (action.kind === "toggle") {
              this.#host.requests.command({ type: "set_mcp_server_enabled", name: action.server, enabled: !action.enabled })
            } else if (action.kind === "review") {
              this.#host.requests.command({ type: "review_mcp_server", name: action.server })
            } else if (action.kind === "approve") {
              this.#host.requests.command({ type: "approve_mcp_server", name: action.server, fingerprint: action.fingerprint })
            } else {
              this.#mcpActionName = action.server
              this.#host.pickerController.kind = "mcpRemoveConfirm"
              this.#host.pickerController.refresh()
            }
          },
        )
        break
      }
      case "mcpRemoveConfirm": {
        const server = this.#mcpActionName
        if (server === null) {
          this.#host.pickerController.kind = "mcp"
          this.#host.pickerController.refresh()
          break
        }
        this.#host.pickerController.show(
          `Remove ${server}? This deletes its configuration`,
          [
            { id: "mcp.remove.confirm", label: "Remove", description: "Disable if needed, then delete", value: true },
            { id: "mcp.remove.cancel", label: "Cancel", description: "Keep this server", value: false },
          ],
          (item) => {
            if (item.value) {
              this.#host.requests.command({ type: "remove_mcp_server", name: server })
              this.#mcpActionName = null
              this.#host.pickerController.kind = "mcp"
            } else {
              this.#host.pickerController.kind = "mcpActions"
            }
            this.#host.pickerController.refresh()
          },
        )
        break
      }
    }
  }
}
