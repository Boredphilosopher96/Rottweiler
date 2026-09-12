export interface BoundedCommandTextProjection {
  readonly lines: readonly string[]
  readonly omittedLineCount: number
}

export interface StructuredCommandResultRow {
  readonly prefixes: readonly ("bullet" | "indent")[]
  readonly label: string | null
  readonly value: StructuredCommandResultValue
}

export type StructuredCommandResultValue =
  | { readonly kind: "heading" }
  | { readonly kind: "string"; readonly value: string }
  | { readonly kind: "number"; readonly value: number }
  | { readonly kind: "boolean"; readonly value: boolean }
  | { readonly kind: "none" }
  | { readonly kind: "empty_list" }
  | { readonly kind: "redacted" }
  | { readonly kind: "details_omitted" }

/** Bounded semantic content retained for a completed slash command. */
export type CommandResultProjection =
  | {
      readonly kind: "help"
      readonly commands: readonly {
        readonly usage: string
        readonly description: string
      }[]
      readonly omittedCommandCount: number
      readonly fallback: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "status"
      readonly agent: string
      readonly mode: string
      readonly queuedMessages: string
    }
  | {
      readonly kind: "permissions"
      readonly summary: string | null
      readonly mode: string | null
      readonly defaultPermission: string | null
      readonly rememberedApprovals: string | null
      readonly rules: readonly {
        readonly scope: "Project" | "Session"
        readonly decision: string
        readonly target: string
        readonly remembered: boolean
      }[]
      readonly omittedRuleCount: number
    }
  | {
      readonly kind: "mode"
      readonly mode: string | null
      readonly active: boolean
    }
  | {
      readonly kind: "plan"
      readonly title: string | null
      readonly body: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "review"
      readonly summary: string | null
      readonly files: readonly {
        readonly path: string
        readonly status: string
        readonly note: string
      }[]
      readonly omittedFileCount: number
    }
  | {
      readonly kind: "trust"
      readonly trust: "updated" | "trusted" | "untrusted" | "unknown"
      readonly message: string | null
    }
  | {
      readonly kind: "mcp"
      readonly updated: boolean
      readonly servers: readonly {
        readonly name: string
        readonly status: string
      }[]
      readonly omittedServerCount: number
      readonly fallback: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "completion"
      readonly title: string
      readonly detail: string | null
    }
  | {
      readonly kind: "message"
      readonly content: BoundedCommandTextProjection
    }
  | {
      readonly kind: "structured"
      readonly rows: readonly StructuredCommandResultRow[]
      readonly omittedRowCount: number
    }
  | { readonly kind: "unsafe_structured" }

