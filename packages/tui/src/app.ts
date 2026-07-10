import { BoxRenderable, TextRenderable, type RenderContext } from "@opentui/core"

import type { ClientCommand, EngineEvent } from "./protocol"

export interface RottweilerAppOptions {
  initialEvent?: EngineEvent
  onCommand?: (command: ClientCommand) => void
}

/**
 * Build the M0 shell with OpenTUI's retained renderable tree.
 *
 * The protocol-shaped options are deliberately present from the first screen:
 * future engine integration extends this seam instead of creating a UI-only
 * state channel.
 */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions = {},
): BoxRenderable {
  const frame = new BoxRenderable(renderer, {
    id: "rottweiler-app",
    width: "100%",
    height: "100%",
    padding: 1,
    gap: 1,
    flexDirection: "column",
    border: true,
    borderStyle: "rounded",
    borderColor: "#E6B450",
    title: " Rottweiler ",
    titleColor: "#F7C56B",
  })

  frame.add(
    new TextRenderable(renderer, {
      id: "mission",
      content: "Fast, headless-first coding agent harness",
      fg: "#F2F4F8",
    }),
  )

  frame.add(
    new TextRenderable(renderer, {
      id: "protocol-status",
      content:
        options.initialEvent === undefined
          ? "Generated protocol connected · waiting for engine"
          : "Generated protocol connected · event received",
      fg: "#78DCE8",
    }),
  )

  frame.add(
    new TextRenderable(renderer, {
      id: "hint",
      content: "M0 OpenTUI renderer spike",
      fg: "#A9B1D6",
    }),
  )

  return frame
}
