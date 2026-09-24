import { expect, test } from "bun:test"
import { contextWarning } from "../src/state/context-usage"

const usage = { through: null, turn_id: null, stable_prefix_hash: "fixture", used_tokens: "0", usable_tokens: "10000", reserved_tokens: "1000", context_window_known: true }
test("context warnings use known usable capacity at both thresholds", () => {
  expect(contextWarning(null)).toBeNull()
  expect(contextWarning({ ...usage, used_tokens: "6999" })).toBeNull()
  expect(contextWarning({ ...usage, used_tokens: "7000" })).toContain("Context filling")
  expect(contextWarning({ ...usage, used_tokens: "8499" })).toContain("Context filling")
  expect(contextWarning({ ...usage, used_tokens: "8500" })).toContain("Context near limit")
  expect(contextWarning({ ...usage, used_tokens: "9000", context_window_known: false })).toBeNull()
  expect(contextWarning({ ...usage, usable_tokens: "0" })).toBeNull()
})
