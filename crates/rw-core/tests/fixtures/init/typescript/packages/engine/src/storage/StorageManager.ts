import type { Session } from 'electron'

/**
 * Foundation for a real "clear browsing data" pane. Operates on a profile+space
 * session's Chromium storage.
 */
export class StorageManager {
  async cacheSize(ses: Session): Promise<number> {
    return ses.getCacheSize()
  }

  async clearCache(ses: Session): Promise<void> {
    await ses.clearCache()
  }

  async clearStorage(ses: Session): Promise<void> {
    await ses.clearStorageData({
      storages: ['cookies', 'indexdb', 'localstorage', 'cachestorage', 'serviceworkers', 'websql']
    })
  }
}
