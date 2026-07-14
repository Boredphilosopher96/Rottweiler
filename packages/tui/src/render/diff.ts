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

function hasFileHeaders(lines: readonly string[]): boolean {
  return lines.some((line, index) =>
    line.startsWith("--- ") && (lines[index + 1] ?? "").startsWith("+++ "),
  )
}
