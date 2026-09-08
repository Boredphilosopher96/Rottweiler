import { afterAll } from "bun:test"
import { closeTestProcesses } from "./process-settlement"

// Bun's global preload afterAll runs after every test file's cleanup. Bun 1.3
// does not emit Node's process exit event when its test command finishes.
afterAll(() => { closeTestProcesses() })
