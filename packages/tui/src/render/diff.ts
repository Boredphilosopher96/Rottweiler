const ANSI_ESCAPE = /\u001b\[[0-?]*[ -/]*[@-~]/g

/**
 * Repair common producer-side unified-diff count mistakes before OpenTUI's
 * structured renderer sees them. Diff parsers are deliberately strict; their
 * diagnostics are implementation detail and must never become transcript UI.
 */
export function presentableUnifiedDiff(path: string, source: string): string {
  const clean = source
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .replace(ANSI_ESCAPE, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, "")
  const lines = clean.split("\n")
  if (lines.at(-1) === "") lines.pop()
  if (lines.length === 0) return ""

  let firstHunk = lines.findIndex((line) => line.startsWith("@@"))
  if (firstHunk >= 0 && !hasFileHeaders(lines.slice(0, firstHunk))) {
    const displayPath = path === "" ? "file" : path
    lines.unshift(`--- a/${displayPath}`, `+++ b/${displayPath}`)
    firstHunk += 2
  }

  // Some tool producers send file headers followed directly by changed lines,
  // or even a human pseudo-diff with no headers. Normalize either shape to one
  // valid file/hunk before it reaches OpenTUI. This is deliberately more
  // conservative than passing malformed source through and exposing a parser
  // diagnostic in the transcript.
  if (firstHunk < 0) {
    const displayPath = path === "" ? "file" : path
    const body = lines
      .filter((line, index) =>
        !line.startsWith("diff --git ") &&
        !line.startsWith("index ") &&
        !(line.startsWith("--- ") && (lines[index + 1] ?? "").startsWith("+++ ")) &&
        !(line.startsWith("+++ ") && (lines[index - 1] ?? "").startsWith("--- ")),
      )
      .map((line) => line.startsWith("+") || line.startsWith("-") || line.startsWith(" ") || line.startsWith("\\")
        ? line
        : ` ${line}`)
    let oldCount = 0
    let newCount = 0
    for (const line of body) {
      if (line.startsWith("+")) newCount += 1
      else if (line.startsWith("-")) oldCount += 1
      else if (!line.startsWith("\\")) {
        oldCount += 1
        newCount += 1
      }
    }
    return [
      `--- a/${displayPath}`,
      `+++ b/${displayPath}`,
      `@@ -1,${oldCount} +1,${newCount} @@`,
      ...body,
      "",
    ].join("\n")
  }

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index] ?? ""
    if (!line.startsWith("@@")) continue
    const parsed = /^@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s+@@(.*)$/.exec(line)
    const oldStart = parsed?.[1] ?? "1"
    const newStart = parsed?.[2] ?? "1"
    const suffix = parsed?.[3] ?? ""
    let oldCount = 0
    let newCount = 0
    let cursor = index + 1
    for (; cursor < lines.length; cursor += 1) {
      const candidate = lines[cursor] ?? ""
      if (
        candidate.startsWith("@@") ||
        candidate.startsWith("diff --git ") ||
        (candidate.startsWith("--- ") && (lines[cursor + 1] ?? "").startsWith("+++ "))
      ) break
      if (candidate === "") {
        lines[cursor] = " "
        oldCount += 1
        newCount += 1
      } else if (candidate.startsWith("+")) {
        newCount += 1
      } else if (candidate.startsWith("-")) {
        oldCount += 1
      } else if (candidate.startsWith(" ")) {
        oldCount += 1
        newCount += 1
      } else if (!candidate.startsWith("\\")) {
        // Treat an unprefixed producer line as context. This preserves its
        // visible content while keeping the structured parser deterministic.
        lines[cursor] = ` ${candidate}`
        oldCount += 1
        newCount += 1
      }
    }
    lines[index] = `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@${suffix}`
    index = cursor - 1
  }
  return `${lines.join("\n")}\n`
}

/**
 * Produce a valid changed-lines-only diff for the inline transcript card.
 * Context gaps become separate hunks so line numbers remain truthful after
 * removing unchanged lines. Full review surfaces continue to use
 * `presentableUnifiedDiff`.
 */
export function minimalUnifiedDiff(path: string, source: string): string {
  const normalized = presentableUnifiedDiff(path, source)
  const lines = normalized.trimEnd().split("\n")
  const headers = lines.filter((line) => line.startsWith("--- ") || line.startsWith("+++ "))
  const output = headers.slice(0, 2)
  let index = 0
  while (index < lines.length) {
    const header = lines[index] ?? ""
    const parsed = /^@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s+@@(.*)$/.exec(header)
    if (parsed === null) {
      index += 1
      continue
    }
    let oldLine = Number.parseInt(parsed[1] ?? "1", 10)
    let newLine = Number.parseInt(parsed[2] ?? "1", 10)
    const suffix = parsed[3] ?? ""
    index += 1
    let group: string[] = []
    let groupOldStart = oldLine
    let groupNewStart = newLine
    const flush = () => {
      if (group.length === 0) return
      const oldCount = group.filter((line) => line.startsWith("-")).length
      const newCount = group.filter((line) => line.startsWith("+")).length
      // Unified-diff zero-count ranges are anchored on the preceding line.
      // A pure insertion before old line N is `-(N-1),0`; deletion is the
      // symmetric `+(N-1),0`. Replacements retain their actual first line.
      const oldStart = oldCount === 0 ? Math.max(0, groupOldStart - 1) : groupOldStart
      const newStart = newCount === 0 ? Math.max(0, groupNewStart - 1) : groupNewStart
      output.push(
        `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@${suffix}`,
        ...group,
      )
      group = []
    }
    while (index < lines.length && !(lines[index] ?? "").startsWith("@@")) {
      const line = lines[index] ?? ""
      if (line.startsWith("--- ") && (lines[index + 1] ?? "").startsWith("+++ ")) break
      if (line.startsWith(" ") || (!line.startsWith("+") && !line.startsWith("-") && !line.startsWith("\\"))) {
        flush()
        oldLine += 1
        newLine += 1
      } else {
        if (group.length === 0) {
          groupOldStart = oldLine
          groupNewStart = newLine
        }
        if (line.startsWith("-")) oldLine += 1
        else if (line.startsWith("+")) newLine += 1
        if (!line.startsWith("\\") || group.length > 0) group.push(line)
      }
      index += 1
    }
    flush()
  }
  return output.length > 2 ? `${output.join("\n")}\n` : normalized
}

/** Number of terminal rows occupied by OpenTUI's side-by-side diff view. */
export function splitDiffVisualRows(source: string): number {
  const lines = source.trimEnd().split("\n")
  let rows = 0
  let removed = 0
  let added = 0
  const flush = () => {
    rows += Math.max(removed, added)
    removed = 0
    added = 0
  }
  for (const line of lines) {
    if (line.startsWith("@@")) {
      flush()
    } else if (line.startsWith("-") && !line.startsWith("--- ")) {
      removed += 1
    } else if (line.startsWith("+") && !line.startsWith("+++ ")) {
      added += 1
    } else if (!line.startsWith("--- ") && !line.startsWith("+++ ") && !line.startsWith("\\")) {
      flush()
      rows += 1
    }
  }
  flush()
  return Math.max(1, rows)
}

/** Number of terminal rows occupied by OpenTUI's unified diff view. */
export function unifiedDiffVisualRows(source: string): number {
  let rows = 0
  for (const line of source.trimEnd().split("\n")) {
    if (!line.startsWith("--- ") && !line.startsWith("+++ ") && !line.startsWith("@@") && !line.startsWith("\\")) {
      rows += 1
    }
  }
  return Math.max(1, rows)
}

export function diffStats(source: string): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const line of source.split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++ ")) added += 1
    else if (line.startsWith("-") && !line.startsWith("--- ")) removed += 1
  }
  return { added, removed }
}

export function truncateUnifiedDiff(
  source: string,
  maxRows: number,
  rowModel: "split" | "unified" = "split",
): { diff: string; hiddenLines: number } {
  const lines = source.trimEnd().split("\n")
  const firstHunk = lines.findIndex((line) => line.startsWith("@@"))
  if (firstHunk < 0) return { diff: source, hiddenLines: 0 }

  const headers = lines.slice(0, firstHunk)
  const hunks: string[][] = []
  let index = firstHunk
  while (index < lines.length) {
    const end = lines.findIndex((line, offset) => offset > index && line.startsWith("@@"))
    const next = end < 0 ? lines.length : end
    hunks.push(lines.slice(index, next))
    index = next
  }

  const included: string[][] = []
  for (const hunk of hunks) {
    const candidate = [...headers, ...included.flat(), ...hunk].join("\n")
    const rows = rowModel === "unified" ? unifiedDiffVisualRows(candidate) : splitDiffVisualRows(candidate)
    if (rows > maxRows) {
      if (included.length === 0) {
        const retained = truncateFirstHunk(headers, hunk, maxRows, rowModel)
        const hiddenLines = hunk.length - retained.length
          + hunks.slice(1).reduce((count, laterHunk) => count + laterHunk.length, 0)
        return {
          diff: `${[...headers, ...retained].join("\n")}${source.endsWith("\n") ? "\n" : ""}`,
          hiddenLines,
        }
      }
      break
    }
    included.push(hunk)
  }
  if (included.length === hunks.length) return { diff: source, hiddenLines: 0 }

  const hiddenLines = hunks.slice(included.length).reduce((count, hunk) => count + hunk.length, 0)
  return { diff: `${[...headers, ...included.flat()].join("\n")}${source.endsWith("\n") ? "\n" : ""}`, hiddenLines }
}

function truncateFirstHunk(
  headers: readonly string[],
  hunk: readonly string[],
  maxRows: number,
  rowModel: "split" | "unified",
): string[] {
  const retained = [hunk[0] ?? "@@ -1,0 +1,0 @@"]
  for (const line of hunk.slice(1)) {
    const candidate = [...headers, ...retained, line].join("\n")
    const rows = rowModel === "unified" ? unifiedDiffVisualRows(candidate) : splitDiffVisualRows(candidate)
    if (rows > maxRows) break
    retained.push(line)
  }
  return rewriteHunkCounts(retained)
}

function rewriteHunkCounts(hunk: readonly string[]): string[] {
  const header = hunk[0] ?? "@@ -1,0 +1,0 @@"
  const parsed = /^@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s+@@(.*)$/.exec(header)
  const oldStart = parsed?.[1] ?? "1"
  const newStart = parsed?.[2] ?? "1"
  const suffix = parsed?.[3] ?? ""
  let oldCount = 0
  let newCount = 0
  for (const line of hunk.slice(1)) {
    if (line.startsWith("+")) newCount += 1
    else if (line.startsWith("-")) oldCount += 1
    else if (!line.startsWith("\\")) {
      oldCount += 1
      newCount += 1
    }
  }
  return [`@@ -${oldStart},${oldCount} +${newStart},${newCount} @@${suffix}`, ...hunk.slice(1)]
}

function hasFileHeaders(lines: readonly string[]): boolean {
  return lines.some((line, index) =>
    line.startsWith("--- ") && (lines[index + 1] ?? "").startsWith("+++ "),
  )
}
