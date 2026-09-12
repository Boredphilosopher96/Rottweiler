import type {
  BoundedCommandTextProjection,
  CommandResultProjection,
  StructuredCommandResultRow,
} from "./command-types"

export function commandResultMarkdown(projection: CommandResultProjection): string {
  switch (projection.kind) {
    case "help":
      if (projection.commands.length === 0) {
        return projection.fallback === null
          ? "No commands are available."
          : boundedRowsMarkdown(projection.fallback)
      }
      return [
        "| Command | What it does |",
        "| --- | --- |",
        ...projection.commands.map(({ usage, description }) =>
          `| \`${usage}\` | ${description} |`),
        ...(projection.omittedCommandCount === 0
          ? []
          : [`| … | ${projection.omittedCommandCount} more commands |`]),
      ].join("\n")
    case "status":
      return `**${sentenceCase(projection.agent)}** · ${sentenceCase(projection.mode)} mode · ${projection.queuedMessages} queued message${projection.queuedMessages === "1" ? "" : "s"}`
    case "permissions":
      return permissionCommandMarkdown(projection)
    case "mode":
      if (projection.mode === null) return "**Mode unchanged**"
      return projection.active
        ? `**${sentenceCase(projection.mode)} mode** · currently active`
        : `**${sentenceCase(projection.mode)} mode enabled**`
    case "plan":
      if (projection.title === null) return "_No plan has been submitted._"
      return [
        `## ${projection.title.replace(/^#+\s*/, "")}`,
        projection.body === null ? "" : boundedRowsMarkdown(projection.body),
      ].filter(Boolean).join("\n\n")
    case "review":
      if (projection.summary === null) return "**No changed files**"
      return [
        `**${sentenceCase(projection.summary)}**`,
        ...(projection.files.length === 0
          ? []
          : [
              "\n| File | Status | Note |",
              "| --- | --- | --- |",
              ...projection.files.map(({ path, status, note }) =>
                `| \`${markdownCell(path)}\` | ${sentenceCase(status)} | ${markdownCell(note)} |`),
            ]),
        ...(projection.omittedFileCount === 0
          ? []
          : [`\n… ${projection.omittedFileCount} more files · open \`/review\` for the full diff`]),
      ].join("\n")
    case "trust": {
      if (projection.message === null) return "**Folder trust updated**"
      const title = projection.trust === "trusted"
        ? "Folder trusted"
        : projection.trust === "untrusted"
          ? "Folder not trusted"
          : "Folder trust"
      return `**${title}** · ${sentenceCase(projection.message)}`
    }
    case "mcp":
      if (projection.updated) return "**MCP settings updated**"
      if (projection.servers.length === 0) {
        return projection.fallback === null ? "Command completed." : boundedRowsMarkdown(projection.fallback)
      }
      return [
        "| Server | Status |",
        "| --- | --- |",
        ...projection.servers.map(({ name, status }) =>
          `| ${markdownCell(name)} | ${markdownCell(status)} |`),
        ...(projection.omittedServerCount === 0
          ? []
          : [`| … | ${projection.omittedServerCount} more servers |`]),
      ].join("\n")
    case "completion":
      return projection.detail === null
        ? `**${projection.title}**`
        : `**${projection.title}** · ${sentenceCase(projection.detail)}`
    case "message": {
      const rendered = boundedRowsMarkdown(projection.content)
      return rendered === "" ? "Command completed." : rendered
    }
    case "structured": {
      if (projection.rows.length === 0) return "Command completed."
      return boundedRowsMarkdown({
        lines: projection.rows.map(structuredRowText),
        omittedLineCount: projection.omittedRowCount,
      })
    }
    case "unsafe_structured":
      return "_Command returned structured details that could not be displayed safely._"
  }
}

function permissionCommandMarkdown(
  projection: Extract<CommandResultProjection, { readonly kind: "permissions" }>,
): string {
  if (projection.summary !== null) {
    return `**Permissions** · ${sentenceCase(projection.summary)}`
  }
  if (
    projection.mode === null &&
    projection.defaultPermission === null &&
    projection.rememberedApprovals === null &&
    projection.rules.length === 0
  ) return "**Permissions updated**"
  const heading = projection.mode === null
    ? "**Permission settings**"
    : `**${sentenceCase(projection.mode)} permissions**${projection.defaultPermission === null ? "" : ` · ${projection.defaultPermission} by default`}`
  return [
    heading,
    ...(projection.rememberedApprovals === null
      ? []
      : [`Remembered:${projection.rememberedApprovals}`]),
    ...(projection.rules.length === 0
      ? []
      : [
          "\n| Scope | Decision | Applies to |",
          "| --- | --- | --- |",
          ...projection.rules.map((rule) =>
            rule.remembered
              ? `| ${rule.scope} | Remembered | ${markdownCell(humanLabel(rule.target))} |`
              : `| ${rule.scope} | ${sentenceCase(rule.decision)} | \`${markdownCell(rule.target)}\` |`),
        ]),
    ...(projection.omittedRuleCount === 0
      ? []
      : [`\n… ${projection.omittedRuleCount} more rules · open \`/permissions\` to manage`]),
  ].join("\n")
}

function boundedRowsMarkdown(projection: BoundedCommandTextProjection): string {
  return [
    ...projection.lines,
    ...(projection.omittedLineCount === 0
      ? []
      : [`\n… ${projection.omittedLineCount} more lines`]),
  ].join("\n")
}

function structuredRowText(row: StructuredCommandResultRow): string {
  const prefix = row.prefixes.map((item) => item === "bullet" ? "- " : "  ").join("")
  const value = structuredValueText(row)
  if (row.label === null) return `${prefix}${value}`
  const label = humanLabel(row.label)
  return row.value.kind === "heading"
    ? `${prefix}${label}:`
    : `${prefix}${label}: ${value}`
}

function structuredValueText(row: StructuredCommandResultRow): string {
  switch (row.value.kind) {
    case "heading":
      return ""
    case "string":
      return humanEnum(row.value.value)
    case "number":
    case "boolean":
      return String(row.value.value)
    case "none":
      return "none"
    case "empty_list":
      return row.label === null ? "None" : "none"
    case "redacted":
      return "[redacted]"
    case "details_omitted":
      return "details omitted"
  }
}

function markdownCell(value: string): string {
  return value.replaceAll("|", "\\|").replaceAll("`", "'")
}

function sentenceCase(value: string): string {
  if (value.length === 0) return value
  return `${value.slice(0, 1).toUpperCase()}${value.slice(1).replace(/[.!]+$/, "")}`
}

function humanLabel(value: string): string {
  const words = value.replaceAll("_", " ").replaceAll("-", " ")
  return `${words.slice(0, 1).toUpperCase()}${words.slice(1)}`
}

function humanEnum(value: string): string {
  return value.includes("_") && !value.includes(" ") ? value.replaceAll("_", " ") : value
}
