import type { FeatureModule, FeatureContext } from '../Feature'

/**
 * Ghost Tab (spec: GhostTab.md) — watch a page for meaningful changes and notify.
 * The primitive it needs already exists in the base: hidden WebContentsView +
 * hibernation. A Ghost Tab hibernates between polls (no persistent renderer),
 * spins up a hidden view on schedule, extracts the watched element (or body),
 * strips dynamic noise, diffs against the last snapshot, and fires a native
 * notification past a threshold. Min interval floored (be a good web citizen);
 * one concurrent poll at a time. Off by default.
 */
export class GhostTab implements FeatureModule {
  readonly id = 'feature.ghostTab'
  readonly name = 'Ghost Tab'
  readonly description = 'Watch any page in the background and get notified when it changes.'
  readonly experimental = false
  readonly settingKey = 'ghostTabEnabled' as const


  activate(_ctx: FeatureContext): void {
    // TODO: start the poll scheduler for registered watchers (hidden-view diff).
  }

  deactivate(): void {
    // TODO: stop the scheduler and drop watchers.
  }
}
