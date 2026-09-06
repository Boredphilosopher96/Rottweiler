import { composerDraftBytes, MAX_CLIENT_DRAFT_BYTES, MAX_CLIENT_DRAFTS } from "./composer-drafts"
import type { AppClientState } from "./recycle-state"
import type { ComposerDraft } from "./subagent-state"

/** The private payload must fit the same aggregate editing owner used after restart. */
export function admittedRecycleDrafts(state: AppClientState): boolean {
  const parent = state.parentComposer ?? state.composer
  let bytes = composerDraftBytes(parent), count = bytes === 0 ? 0 : 1
  if (bytes > 0) bytes += "parent".length * 2
  const ids = new Set<string>()
  for (const { id, draft } of state.subagentDrafts) {
    if (ids.has(id)) return false
    ids.add(id)
    const held = composerDraftBytes(draft)
    if (held > 0) { bytes += held + `child:${id}`.length * 2; count++ }
    if (bytes > MAX_CLIENT_DRAFT_BYTES || count > MAX_CLIENT_DRAFTS) return false
  }
  if (bytes > MAX_CLIENT_DRAFT_BYTES) return false
  if (state.child !== null) {
    const target = state.child.target
    const ancestry = state.child.type === "live" ? state.child.target.ancestry
      : "scope" in target && target.scope.type === "descendant" ? target.scope.ancestry : []
    const id = ancestry.at(-1)?.subagent_id
    const draft = state.subagentDrafts.find(entry => entry.id === id)?.draft
    if (draft === undefined || !sameDraft(draft, state.composer)) return false
  }
  return true
}

function sameDraft(left: ComposerDraft, right: ComposerDraft): boolean {
  return left.content === right.content && left.attachments.length === right.attachments.length
    && left.attachments.every((attachment, index) => {
      const other = right.attachments[index]!
      return attachment.name === other.name && attachment.media_type === other.media_type && attachment.source_path === other.source_path
        && attachment.data.type === other.data.type && (attachment.data.type === "text"
          ? other.data.type === "text" && attachment.data.content === other.data.content
          : other.data.type === "inline_base64" && attachment.data.data === other.data.data)
    })
}
