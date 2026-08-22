import { afterEach, describe, expect, mock, spyOn, test } from "bun:test"

import { CodeRenderable, type TreeSitterClient } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"

import { createSyntaxStyle, nordTheme } from "../src/theme"
import {
  registerTreeSitterParsersLazily,
  stabilizeTreeSitterClient,
} from "../src/tree-sitter-client"
import { embeddedParserConfigurations } from "../src/tree-sitter-runtime"

interface ControlledHighlighter {
  readonly client: TreeSitterClient
  reject(error: Error): void
}

function controlledHighlighter(): ControlledHighlighter {
  let reject: ((error: Error) => void) | undefined
  return {
    client: {
      highlightOnce: () => new Promise((_, rejectPromise) => {
        reject = rejectPromise
      }),
    } as unknown as TreeSitterClient,
    reject(error) {
      if (reject === undefined) throw new Error("highlight request has not started")
      reject(error)
    },
  }
}

afterEach(() => mock.restore())

describe("OpenTUI highlighting lifecycle", () => {
  test("registers fenced-code grammars on first use instead of at startup", async () => {
    const registered: string[] = []
    const buffers = new Map<number, { filetype: string }>()
    const client = {
      addFiletypeParser(parser: { filetype: string }) {
        registered.push(parser.filetype)
      },
      async highlightOnce() {
        return {}
      },
      async createBuffer(id: number, _content: string, filetype: string) {
        buffers.set(id, { filetype })
        return true
      },
      async resetBuffer() {},
      async updateBuffer() {},
      getBuffer(id: number) {
        return buffers.get(id)
      },
    } as unknown as TreeSitterClient
    const lazy = registerTreeSitterParsersLazily(
      client,
      embeddedParserConfigurations("/tmp/tree-sitter-assets"),
    )

    expect(registered).toEqual([])
    await lazy.highlightOnce("```ts\nconst answer = 42\n```", "markdown")
    await lazy.highlightOnce("```ts\nconst answer = 43\n```", "markdown")

    expect(registered).toEqual(["markdown", "markdown_inline", "typescript"])
  })

  test("does not report expected parser cancellation after a code renderable is destroyed", async () => {
    const warnings: unknown[][] = []
    spyOn(console, "warn").mockImplementation((...arguments_: unknown[]) => {
      warnings.push(arguments_)
    })
    const setup = await createTestRenderer({ width: 40, height: 8, useThread: false })
    const highlighter = controlledHighlighter()
    const syntaxStyle = createSyntaxStyle(nordTheme)
    const code = new CodeRenderable(setup.renderer, {
      content: "const answer = 42",
      filetype: "typescript",
      syntaxStyle,
      treeSitterClient: stabilizeTreeSitterClient(highlighter.client),
    })
    setup.renderer.root.add(code)
    await setup.renderOnce()
    expect(code.isHighlighting).toBe(true)

    code.destroyRecursively()
    highlighter.reject(new Error("TreeSitter client destroyed"))
    await code.highlightingDone

    expect(warnings).toEqual([])
    syntaxStyle.destroy()
    setup.renderer.destroy()
  })

  test("continues to report highlighting failures for a live code renderable", async () => {
    const warnings: unknown[][] = []
    spyOn(console, "warn").mockImplementation((...arguments_: unknown[]) => {
      warnings.push(arguments_)
    })
    const setup = await createTestRenderer({ width: 40, height: 8, useThread: false })
    const highlighter = controlledHighlighter()
    const syntaxStyle = createSyntaxStyle(nordTheme)
    const code = new CodeRenderable(setup.renderer, {
      content: "const answer = 42",
      filetype: "typescript",
      syntaxStyle,
      treeSitterClient: stabilizeTreeSitterClient(highlighter.client),
    })
    setup.renderer.root.add(code)
    await setup.renderOnce()
    highlighter.reject(new Error("parser unavailable"))
    await code.highlightingDone

    expect(warnings).toHaveLength(1)
    expect(warnings[0]?.[0]).toBe("Code highlighting failed, falling back to plain text:")
    expect(String(warnings[0]?.[1])).toContain("parser unavailable")
    setup.renderer.destroy()
    syntaxStyle.destroy()
  })
})
