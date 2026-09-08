import type { RottweilerApp } from "../app"
import type { ChildUiController } from "./children"
import type { DocumentController } from "../history/document"

/** Navigation changes the visible source while preserving drafts and interaction priority. */
export async function navigateTranscript(
  app: RottweilerApp, children: ChildUiController, document: DocumentController,
  closeReview: () => void, source: string | import("../protocol").SessionSearchMatch,
): Promise<import("../protocol").TranscriptAnchor | null> {
  children.leaveSubagent()
  if (children.activeId !== null) throw new Error("Keep or submit the child draft before leaving its session.")
  app.closePicker("scope_change")
  document.close()
  closeReview()
  app.showConversationView()
  app.setState(app.state)
  if (typeof source === "string") return app.transcript.revealHistorySource(source)
  await app.transcript.revealSearchMatch(source)
  return null
}
