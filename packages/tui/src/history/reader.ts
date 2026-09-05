import type { TranscriptContentPage, TranscriptContentRead, TranscriptRead, TranscriptReadResult } from "../protocol"

/** Read-only capability shared by live sessions and the offline historical view. */
export interface HistoryReader {
  page(sessionId: string, read: TranscriptRead, signal: AbortSignal): Promise<TranscriptReadResult>
  content(sessionId: string, read: TranscriptContentRead, signal: AbortSignal): Promise<TranscriptContentPage>
}
