import type { PermissionModeDescriptor } from "../protocol"
import type { RottweilerState } from "../state"

export const KNOWN_TOOL_DISPLAY_NAMES: Readonly<Record<string, string>> = {
  bash: "Terminal command",
  glob: "Find files",
  grep: "Search files",
  ls: "List files",
  read: "Read file",
  write: "Write file",
  edit: "Edit file",
  multi_edit: "Edit files",
  webfetch: "Open web page",
  websearch: "Search the web",
  ask_user: "Ask a question",
  todo: "Update tasks",
}

export function toolDisplayName(name: string): string {
  return KNOWN_TOOL_DISPLAY_NAMES[name] ?? humanLabel(name)
}

export function permissionRuntimeMode(
  permissions: RottweilerState["permissions"],
): PermissionModeDescriptor | null {
  return permissions?.runtime_mode ?? null
}

export function humanLabel(value: string): string {
  return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase())
}
