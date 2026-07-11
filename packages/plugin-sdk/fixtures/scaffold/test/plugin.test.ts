import { expect, test } from "bun:test"
import { plugin } from "../src/index"

test("declares a fail-closed pre_tool hook and custom tool", () => {
  expect(plugin.manifest.capabilities.tools?.[0]?.name).toBe("hello")
  expect(plugin.manifest.capabilities.hooks?.[0]).toEqual({
    name: "pre_tool",
    failure_policy: "fail-closed",
  })
})
