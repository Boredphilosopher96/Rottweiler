import { MarkdownRenderable, SyntaxStyle, addDefaultParsers, destroyTreeSitterClient, getTreeSitterClient } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"
import { writeFile } from "node:fs/promises"

import { embeddedParserConfigurations, materializeTreeSitterRuntime } from "./tree-sitter-runtime"
import { stabilizeTreeSitterClient } from "./tree-sitter-client"

/** Compiled-executable acceptance used by release tests; not part of normal startup. */
export async function runCompiledTreeSitterSmoke(reportPath: string): Promise<void> {
  const runtime = await materializeTreeSitterRuntime()
  process.env.OTUI_ASSET_ROOT = runtime.root
  process.env.OTUI_TREE_SITTER_WORKER_PATH = runtime.workerPath
  addDefaultParsers(embeddedParserConfigurations(runtime.assetsPath))
  const client = stabilizeTreeSitterClient(getTreeSitterClient())
  const setup = await createTestRenderer({ width: 64, height: 30, useThread: false })
  const syntax = SyntaxStyle.fromStyles({
    default: { fg: "#D8DEE9" },
    "markup.heading": { fg: "#EBCB8B", bold: true },
    "markup.bold": { fg: "#FFFFFF", bold: true },
    keyword: { fg: "#88C0D0", bold: true },
    number: { fg: "#B48EAD" },
  })
  try {
    await client.initialize()
    const markdown = new MarkdownRenderable(setup.renderer, {
      content: [
        "## Embedded result",
        "",
        "**complete**",
        "",
        "```typescript",
        "const answer = 42",
        "```",
        "",
        "```bash",
        "printf '%s\\n' \"$HOME\"",
        "```",
        "",
        "```rust",
        "fn answer() -> u32 { 42 }",
        "```",
        "",
        "```lua",
        "local answer = 42",
        "```",
        "",
        "```make",
        "all:",
        "\t@echo ready",
        "```",
      ].join("\n"),
      syntaxStyle: syntax,
      treeSitterClient: client,
      conceal: true,
      concealCode: false,
      width: "100%",
      height: 26,
    })
    setup.renderer.root.add(markdown)
    let frame = ""
    let codeColors: string[] = []
    let ready = false
    const deadline = performance.now() + 10_000
    do {
      await Bun.sleep(10)
      await setup.renderOnce()
      frame = setup.captureCharFrame()
      codeColors = setup.captureSpans().lines
        .flatMap((line) => line.spans)
        .filter((span) => span.text.includes("const") || span.text.includes("42"))
        .map((span) => span.fg.toInts().join(","))
      ready = !(
        !frame.includes("Embedded result") ||
        !frame.includes("complete") ||
        !frame.includes("const answer = 42") ||
        !frame.includes("printf") ||
        !frame.includes("fn answer") ||
        !frame.includes("local answer = 42") ||
        !frame.includes("@echo ready") ||
        frame.includes("## Embedded result") ||
        frame.includes("**complete**") ||
        frame.includes("```typescript") ||
        frame.includes("```bash") ||
        frame.includes("```rust") ||
        frame.includes("```lua") ||
        frame.includes("```make") ||
        new Set(codeColors).size < 2
      )
    } while (!ready && performance.now() < deadline)
    await writeFile(reportPath, JSON.stringify({ frame, codeColors }), { flag: "wx", mode: 0o600 })
    if (!ready) {
      throw new Error("embedded Tree-sitter runtime did not render concealed highlighted Markdown")
    }
  } finally {
    setup.renderer.destroy()
    syntax.destroy()
    await destroyTreeSitterClient()
    delete process.env.OTUI_ASSET_ROOT
    delete process.env.OTUI_TREE_SITTER_WORKER_PATH
  }
}
