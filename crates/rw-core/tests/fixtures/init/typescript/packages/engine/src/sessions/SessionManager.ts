import { session, type Session } from 'electron'

/**
 * One persistent partition per (profile, space): cookies, IndexedDB, Cache
 * Storage, service workers and localStorage are all isolated and durable - this
 * is Chromium's real multi-file storage, gained for free from a persist:
 * partition. Permission handlers are attached by the Browser (they depend on the
 * owning profile's policy + the window's prompt queue), not here.
 */
export class SessionManager {
  private cache = new Map<string, Session>()
  private owners = new Map<string, string>() // key -> profileId

  forSpace(
    profileId: string,
    spaceId: string,
    ephemeral = false
  ): { session: Session; created: boolean } {
    // no `persist:` prefix -> in-memory partition, wiped when the app exits (private spaces)
    const key = ephemeral ? `p:${profileId}:s:${spaceId}` : `persist:p:${profileId}:s:${spaceId}`
    const existing = this.cache.get(key)
    if (existing) return { session: existing, created: false }
    const s = session.fromPartition(key)
    this.cache.set(key, s)
    this.owners.set(key, profileId)
    return { session: s, created: true }
  }

  /** all live partition sessions belonging to a profile (for clear-browsing-data). */
  sessionsFor(profileId: string): Session[] {
    const result: Session[] = []
    for (const [key, owner] of this.owners) {
      if (owner === profileId) {
        const s = this.cache.get(key)
        if (s) result.push(s)
      }
    }
    return result
  }
}
