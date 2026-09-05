import { emptySessionReader } from "../fixtures/history"
import { createTestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../../src/app"

const setup = await createTestRenderer({
  width: 100,
  height: 30,
  useThread: false,
})
const app = createRottweilerApp(setup.renderer, { sessionReader: emptySessionReader })
setup.renderer.root.add(app)
await setup.renderOnce()

process.stdout.write("ROTTWEILER_FIRST_PAINT\n")
setup.renderer.destroy()

