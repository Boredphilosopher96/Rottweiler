import { describe, expect, test } from "bun:test"

import { commandResultMarkdown } from "../src/render"
import type { CommandResultProjection } from "../src/render/command-types"

describe("command result presentation", () => {
  test("renders every structured projection variant as Markdown", () => {
    const fixtures: readonly [CommandResultProjection, string][] = [
      [{
        kind: "help",
        commands: [{ usage: "/status", description: "Show agent status" }],
        omittedCommandCount: 2,
        fallback: null,
      }, [
        "| Command | What it does |",
        "| --- | --- |",
        "| `/status` | Show agent status |",
        "| … | 2 more commands |",
      ].join("\n")],
      [{ kind: "status", agent: "working", mode: "execute", queuedMessages: "2" },
        "**Working** · Execute mode · 2 queued messages"],
      [{
        kind: "permissions",
        summary: null,
        mode: "yolo",
        defaultPermission: "allow",
        rememberedApprovals: " 1 for this session, 0 for this project",
        rules: [{ scope: "Project", decision: "deny", target: "bash(rm *)", remembered: false }],
        omittedRuleCount: 2,
      }, [
        "**Yolo permissions** · allow by default",
        "Remembered: 1 for this session, 0 for this project",
        "",
        "| Scope | Decision | Applies to |",
        "| --- | --- | --- |",
        "| Project | Deny | `bash(rm *)` |",
        "",
        "… 2 more rules · open `/permissions` to manage",
      ].join("\n")],
      [{ kind: "mode", mode: "plan", active: false }, "**Plan mode enabled**"],
      [{
        kind: "plan",
        title: "Ship safely",
        body: { lines: ["Keep state durable.", "1. Update UI"], omittedLineCount: 1 },
      }, "## Ship safely\n\nKeep state durable.\n1. Update UI\n\n… 1 more lines"],
      [{
        kind: "review",
        summary: "Session review: 2 changed file(s) · 1 awaiting review",
        files: [{ path: "src/app.ts", status: "needs review", note: "" }],
        omittedFileCount: 1,
      }, [
        "**Session review: 2 changed file(s) · 1 awaiting review**",
        "",
        "| File | Status | Note |",
        "| --- | --- | --- |",
        "| `src/app.ts` | Needs review |  |",
        "",
        "… 1 more files · open `/review` for the full diff",
      ].join("\n")],
      [{
        kind: "trust",
        trust: "trusted",
        message: "folder trust granted for this workspace",
      }, "**Folder trusted** · Folder trust granted for this workspace"],
      [{
        kind: "mcp",
        updated: false,
        servers: [{ name: "docs", status: "ready · 4 tools" }],
        omittedServerCount: 1,
        fallback: null,
      }, [
        "| Server | Status |",
        "| --- | --- |",
        "| docs | ready · 4 tools |",
        "| … | 1 more servers |",
      ].join("\n")],
      [{ kind: "completion", title: "Compaction started", detail: "compaction started" },
        "**Compaction started** · Compaction started"],
      [{
        kind: "message",
        content: { lines: ["first", "second"], omittedLineCount: 2 },
      }, "first\nsecond\n\n… 2 more lines"],
      [{
        kind: "structured",
        rows: [
          { prefixes: [], label: "paths", value: { kind: "heading" } },
          { prefixes: ["bullet"], label: null, value: { kind: "string", value: "src/main.rs" } },
          { prefixes: [], label: "approval_state", value: { kind: "string", value: "approval_required" } },
        ],
        omittedRowCount: 1,
      }, "Paths:\n- src/main.rs\nApproval state: approval required\n\n… 1 more lines"],
      [{ kind: "unsafe_structured" },
        "_Command returned structured details that could not be displayed safely._"],
    ]

    for (const [projection, expected] of fixtures) {
      expect(commandResultMarkdown(projection)).toBe(expected)
    }
  })
})
