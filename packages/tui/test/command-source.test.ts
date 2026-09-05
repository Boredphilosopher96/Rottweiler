import { expect, test } from "bun:test"
import { projectCommandResult } from "../src/render/command-results"

test("projects built-in command results as bounded semantic content", () => {
  const fixtures = [
    ["help", "/status — Show agent status\n/mode [execute] — Switch mode", {
      kind: "help",
      commands: [
        { usage: "/status", description: "Show agent status" },
        { usage: "/mode [execute]", description: "Switch mode" },
      ],
      omittedCommandCount: 0,
      fallback: null,
    }],
    ["status", "Agent: working\nQueued messages: 2\nMode: execute", {
      kind: "status", agent: "working", mode: "execute", queuedMessages: "2",
    }],
    ["mode", "mode changed to plan", { kind: "mode", mode: "plan", active: false }],
    ["permissions", "Permission mode: yolo\nDefault permission: allow\nConfigured rules:\n- deny · bash(rm *)\nSession rules: none\nRemembered approvals: 1 for this session, 0 for this project", {
      kind: "permissions",
      summary: null,
      mode: "yolo",
      defaultPermission: "allow",
      rememberedApprovals: " 1 for this session, 0 for this project",
      rules: [{ scope: "Project", decision: "deny", target: "bash(rm *)", remembered: false }],
      omittedRuleCount: 0,
    }],
    ["plan", "Ship safely\nKeep state durable.\n1. Update UI\n   Verify: bun test", {
      kind: "plan",
      title: "Ship safely",
      body: { lines: ["Keep state durable.", "1. Update UI", "   Verify: bun test"], omittedLineCount: 0 },
    }],
    ["review", "Session review: 2 changed file(s) · 1 awaiting review\n- src/app.ts · needs review\n- src/lib.rs · accepted", {
      kind: "review",
      summary: "Session review: 2 changed file(s) · 1 awaiting review",
      files: [
        { path: "src/app.ts", status: "needs review", note: "" },
        { path: "src/lib.rs", status: "accepted", note: "" },
      ],
      omittedFileCount: 0,
    }],
    ["trust", "folder trust granted for this workspace", {
      kind: "trust", trust: "trusted", message: "folder trust granted for this workspace",
    }],
    ["mcp", "docs · ready · 4 tools\nsearch · disabled · 0 tools", {
      kind: "mcp",
      updated: false,
      servers: [
        { name: "docs", status: "ready · 4 tools" },
        { name: "search", status: "disabled · 0 tools" },
      ],
      omittedServerCount: 0,
      fallback: null,
    }],
    ["compact", "compaction started", {
      kind: "completion", title: "Compaction started", detail: "compaction started",
    }],
    ["interrupt", "interrupt requested", {
      kind: "completion", title: "Interrupt requested", detail: "interrupt requested",
    }],
    ["rewind", "rewound to turn 4", {
      kind: "completion", title: "Session rewound", detail: "rewound to turn 4",
    }],
    ["add-dir", "added workspace root @root/2", {
      kind: "completion", title: "Workspace updated", detail: "added workspace root @root/2",
    }],
  ] as const
  for (const [name, message, expected] of fixtures) {
    expect(projectCommandResult(name, message)).toEqual(expected)
  }
  const projection = projectCommandResult("extension-report", JSON.stringify({
    data: { entries: Array.from({ length: 80 }, (_, index) => ({ label: `entry-${index}` })), stable_prefix_hash: "private" },
    truncated: false,
  }))
  expect(projection.kind).toBe("structured")
  if (projection.kind !== "structured") throw new Error("expected structured projection")
  expect(projection.rows).toHaveLength(24)
  expect(projection.omittedRowCount).toBe(57)
  expect(JSON.stringify(projection)).not.toContain("stable_prefix_hash")
  expect(JSON.stringify(projection)).not.toContain("private")
})
