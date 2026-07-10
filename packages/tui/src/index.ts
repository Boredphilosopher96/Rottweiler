import { createCliRenderer } from "@opentui/core"

import { createRottweilerApp } from "./app"

const renderer = await createCliRenderer({
  exitOnCtrlC: true,
  targetFps: 60,
})

renderer.root.add(createRottweilerApp(renderer))
