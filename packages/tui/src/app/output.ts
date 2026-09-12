import type { OutputViewerRenderable } from "../components"
import type { DocumentController } from "../history/document"
import type { SessionReadTarget } from "../session-reader"
import type { RottweilerState } from "../state"

/** Live invocation inspection transfers to canonical content at completion. */
export function updateOutputViewer(
  viewer: OutputViewerRenderable, document: DocumentController, target: SessionReadTarget,
  state: RottweilerState, invocationId: string | null,
): string | null {
  if (invocationId === null) return null
  const tool = state.tools[invocationId]
  if (tool === undefined) { viewer.closePresentation(); return null }
  if (tool.status === "finished" && tool.source !== null) {
    viewer.closePresentation()
    void document.openSource(target, tool.source)
    return null
  }
  if (viewer.invocationId === invocationId) viewer.update(tool)
  else viewer.open(tool)
  return invocationId
}
