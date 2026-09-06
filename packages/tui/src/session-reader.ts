import type { SessionChildrenResult } from "../../../protocol/types"
import type { TailRead } from "./history/live-tail"
import type { ReplyAllocation } from "./transport/reply-allocation"
import type { TranscriptContentPage, TranscriptContentRead, TranscriptRead, TranscriptReadResult, TodoReadResult, UiCatalog, UiPanels, SessionReadScope, SessionReadAncestor } from "./protocol"
import { MAX_SESSION_READ_ANCESTORS } from "./protocol"

/** The exact authority travels with a read, independently from the active driver. */
export interface SessionReadTarget {
  readonly sessionId: string
  readonly scope: SessionReadScope
}
export function directSessionRead(sessionId: string): SessionReadTarget {
  return { sessionId, scope: { type: "session" } }
}
export function descendantSessionRead(parent: SessionReadTarget, child: SessionReadAncestor): SessionReadTarget {
  const root = parent.scope.type === "session" ? parent.sessionId : parent.scope.root_session_id
  const ancestry = parent.scope.type === "session" ? [] : parent.scope.ancestry
  if (ancestry.length >= MAX_SESSION_READ_ANCESTORS || child.session_id === root
    || ancestry.some(prior => prior.session_id === child.session_id)) throw new Error("Child history exceeds the permitted ancestry path.")
  return { sessionId: child.session_id, scope: { type: "descendant", root_session_id: root, ancestry: [...ancestry, child] } }
}

/** Read-only capability shared by live sessions and the offline historical view. */
export interface SessionReader {
  tail: TailRead
  children(target: SessionReadTarget, signal: AbortSignal, allocation: ReplyAllocation): Promise<SessionChildrenResult>
  uiCatalog(sessionId: string, signal: AbortSignal): Promise<UiCatalog>
  uiPanels(sessionId: string, signal: AbortSignal): Promise<UiPanels>
  todos(target: SessionReadTarget, signal: AbortSignal): Promise<TodoReadResult>
  page(target: SessionReadTarget, read: TranscriptRead, signal: AbortSignal, allocation: ReplyAllocation): Promise<TranscriptReadResult>
  content(target: SessionReadTarget, read: TranscriptContentRead, signal: AbortSignal, allocation: ReplyAllocation): Promise<TranscriptContentPage>
}
