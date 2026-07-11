import { Store } from '../persistence/Store'
import type { LayoutState } from '@shared/types'

export interface RestoreTab {
  spaceId: string
  url: string
  title: string
}

export interface RestoreData {
  tabs: RestoreTab[]
  activeSpaceId: string
  layout: LayoutState
}

interface RestoreFile {
  data: RestoreData | null
}

/**
 * Optional last-session persistence, per profile. Off unless settings.sessionRestore.
 */
export class SessionRestore {
  private store: Store<RestoreFile>

  constructor(dir: string) {
    this.store = new Store<RestoreFile>(dir, 'session', { data: null })
  }

  save(data: RestoreData): void {
    this.store.replace({ data })
  }

  load(): RestoreData | null {
    return this.store.get().data
  }

  clear(): void {
    this.store.replace({ data: null })
  }
}
