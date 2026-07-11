import type { SyncAdapter, SyncableSnapshot } from '@shared/sync'

/**
 * SKELETON. Syncs a profile snapshot to Google Drive's hidden appDataFolder,
 * so two Sora installs signed into the same Google account converge - cleaner
 * than a bespoke sync server because Drive is the source of truth.
 *
 * Not functional until wired to an OAuth client:
 *  1. Create OAuth credentials (Drive API, scope drive.appdata) in Google Cloud.
 *  2. Do the desktop OAuth loopback flow; store the refresh token in the OS keychain.
 *  3. Implement pull()/push() against files.list/get/create/update in appDataFolder.
 *
 * Credentials are never hardcoded here and are never entered by the assistant -
 * the owner supplies their own client and completes the consent flow.
 */
export class GoogleDriveSync implements SyncAdapter {
  readonly id = 'google-drive'

  constructor(private readonly getAccessToken: () => Promise<string>) {}

  async pull(_profileId: string): Promise<SyncableSnapshot | null> {
    await this.getAccessToken()
    throw new Error('GoogleDriveSync not configured: supply an OAuth client and implement Drive appDataFolder access')
  }

  async push(_snapshot: SyncableSnapshot): Promise<void> {
    await this.getAccessToken()
    throw new Error('GoogleDriveSync not configured: supply an OAuth client and implement Drive appDataFolder access')
  }
}
