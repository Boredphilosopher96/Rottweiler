import { createHash } from "node:crypto"
import type { Attachment } from "../protocol"

const CHUNK = "handoff attachment\n".repeat(256)
const CHUNKS = 1024
export const HANDOFF_ATTACHMENT_BYTES = Buffer.byteLength(CHUNK) * CHUNKS
const expected = createHash("sha256")
for (let index = 0; index < CHUNKS; index++) expected.update(CHUNK)
const DIGEST = expected.digest("hex")

/** A legal text attachment makes the serialized handoff exceed four MiB. */
export function handoffAttachments(): Attachment[] {
  return [{ name: "handoff-notes.txt", media_type: "text/plain", data: { type: "text", content: CHUNK.repeat(CHUNKS) } }]
}

export function verifyHandoffAttachments(attachments: readonly Attachment[]): void {
  const item = attachments[0]
  if (attachments.length !== 1 || item?.name !== "handoff-notes.txt" || item.media_type !== "text/plain"
    || item.data.type !== "text" || Buffer.byteLength(item.data.content) !== HANDOFF_ATTACHMENT_BYTES
    || createHash("sha256").update(item.data.content).digest("hex") !== DIGEST) {
    throw new Error("process handoff lost or changed an accepted attachment")
  }
}
