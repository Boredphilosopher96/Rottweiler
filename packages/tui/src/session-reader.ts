import type { TranscriptContentPage, TranscriptContentRead, TranscriptRead, TranscriptReadResult, TodoReadResult } from "./protocol"

/** Read-only capability shared by live sessions and the offline historical view. */
export interface SessionReader {
  todos(sessionId: string, signal: AbortSignal): Promise<TodoReadResult>
  page(sessionId: string, read: TranscriptRead, signal: AbortSignal): Promise<TranscriptReadResult>
  content(sessionId: string, read: TranscriptContentRead, signal: AbortSignal): Promise<TranscriptContentPage>
}
