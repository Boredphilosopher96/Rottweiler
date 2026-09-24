import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION, type ExtensionInventoryEntry } from "../../../../protocol/types"
import { createRottweilerApp } from "../../src/app"
import { extensionSourceLabel } from "../../src/app/command-catalog"
import type { ClientCommand } from "../../src/protocol"
import { createSkillsBrowserModel } from "../../src/skills-browser"
import { emptySessionReader } from "../fixtures/history"

const SESSION = "skills-session"

const INVENTORY: readonly ExtensionInventoryEntry[] = [
  {
    kind: "skill", name: "review", description: "Review a change for regressions", scope: "user",
    location: ".agents", source_path: "/home/me/.agents/skills/review/SKILL.md", status: "loaded", notes: [],
  },
  {
    kind: "skill", name: null, description: "", scope: "project",
    location: ".claude", source_path: "/repo/.claude/skills/broken/SKILL.md", status: "skipped",
    notes: ["frontmatter is missing the required `description` field"],
  },
  {
    kind: "command", name: "deploy", description: "", scope: "user",
    location: ".rottweiler", source_path: "/home/me/.rottweiler/commands/deploy.md", status: "shadowed",
    notes: ["hidden by the higher-precedence command at /repo/.agents/commands/deploy.md"],
  },
  {
    kind: "agent", name: "auditor", description: "", scope: "project",
    location: ".agents", source_path: "/repo/.agents/agents/auditor.md", status: "untrusted",
    notes: ["project is not trusted; trust the folder to load it"],
  },
]

describe("Skills screen", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("/skills lists every artifact with its source and status and inserts loaded skills", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      sessionId: SESSION,
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "/skills"
    await app.composer.submit()
    await Bun.sleep(0)
    expect(app.skillsBrowser.visible).toBeTrue()
    expect(app.skillsBrowser.heading.plainText).toContain("SKILLS")
    const list = emitted.find((command) => command.type === "list_extensions")
    expect(list).toMatchObject({ type: "list_extensions", session_id: SESSION })

    app.handleEvent({
      type: "extensions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui", request_id: "stale", emitted_at: "2026-01-01T00:00:00Z" },
      session_id: SESSION, entries: [...INVENTORY], truncated: false,
    })
    expect(app.skillsBrowser.itemIds).toEqual([])
    app.handleEvent({
      type: "extensions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui", request_id: list!.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: SESSION, entries: [...INVENTORY], truncated: false,
    })
    expect(app.skillsBrowser.sectionLabels).toEqual(["Skills", "Commands", "Agents"])
    expect(app.skillsBrowser.itemIds).toHaveLength(4)
    expect(app.skillsBrowser.footer.plainText).toContain("Enter insert · 1 loaded · 3 need attention")
    expect(app.skillsBrowser.detail.plainText).toContain("/home/me/.agents/skills/review/SKILL.md")

    app.skillsBrowser.moveSelection(1)
    expect(app.skillsBrowser.detail.plainText).toContain("skipped")
    expect(app.skillsBrowser.detail.plainText).toContain("missing the required `description` field")
    expect(app.skillsBrowser.detail.plainText).toContain("Fix the file named above")
    expect(app.skillsBrowser.activateSelected()).toBeTrue()
    expect(app.skillsBrowser.visible).toBeTrue()

    app.skillsBrowser.moveSelection(2)
    expect(app.skillsBrowser.detail.plainText).toContain("Trust this folder")

    app.skillsBrowser.moveToBoundary(false)
    app.skillsBrowser.activateSelected()
    await Bun.sleep(0)
    expect(app.skillsBrowser.visible).toBeFalse()
    expect(app.composer.value).toBe("/review ")
  })

  test("a failed inventory stays retryable", () => {
    const model = createSkillsBrowserModel({ inventory: { kind: "error", message: "engine offline" }, query: "", selectedId: null })
    expect(model.rows.map((row) => row.id)).toEqual(["skills.retry"])
    expect(model.notice).toEqual({ message: "engine offline", tone: "error" })
  })

  test("palette rows name the kind and scope of declarative extensions", () => {
    expect(extensionSourceLabel({ name: "review", source: "skill", scope: "user" })).toBe("skill · user")
    expect(extensionSourceLabel({ name: "deploy", source: "project", scope: "project" })).toBe("command · project")
    expect(extensionSourceLabel({ name: "legacy", source: "skill", scope: null })).toBe("skill")
    expect(extensionSourceLabel({ name: "mcp.github.triage", source: "mcp", scope: null })).toBe("mcp · github")
  })
})
