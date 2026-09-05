import type { TranscriptContentPage, TranscriptContentRead, TranscriptRead, TranscriptReadResult, TodoReadResult, UiCatalog, UiPanels } from "./protocol"

/** Read-only capability shared by live sessions and the offline historical view. */
export interface SessionReader {
  uiCatalog(sessionId: string, signal: AbortSignal): Promise<UiCatalog>
  uiPanels(sessionId: string, signal: AbortSignal): Promise<UiPanels>
  todos(sessionId: string, signal: AbortSignal): Promise<TodoReadResult>
  page(sessionId: string, read: TranscriptRead, signal: AbortSignal): Promise<TranscriptReadResult>
  content(sessionId: string, read: TranscriptContentRead, signal: AbortSignal): Promise<TranscriptContentPage>
}
