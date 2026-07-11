import type { FeatureModule, FeatureContext } from '../Feature'

/**
 * Reading Position Sync (spec: ReadingPositionSync.md) — scroll position + reading
 * list synced across devices via a user-controlled file (Syncthing/iCloud/Dropbox/
 * USB — the user's choice), no account, no server. Rides the sync layer + Reader
 * Mode. Saves a normalized 0–1 scrollY per URL to `sora-reading.json` in a folder
 * the user points at; a filesystem watcher picks up other devices' writes. On
 * revisiting a saved URL, offers a light "Resume from 61%?" toast. Off by default.
 */
export class ReadingPositionSync implements FeatureModule {
  readonly id = 'feature.readingPosition'
  readonly name = 'Reading Position Sync'
  readonly description = 'Pick up articles where you left off on another device, via a file you control.'
  readonly experimental = false
  readonly settingKey = 'readingPositionEnabled' as const


  activate(_ctx: FeatureContext): void {
    // TODO: watch the reading file; save/restore normalized scroll per URL.
  }

  deactivate(): void {
  }
}
