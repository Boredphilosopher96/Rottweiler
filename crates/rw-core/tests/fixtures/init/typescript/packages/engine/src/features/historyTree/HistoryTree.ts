import type { FeatureModule, FeatureContext, NavigationEvent } from '../Feature'

/**
 * History Tree (spec: HistoryTree.md) — a per-session spatial map of tab ancestry.
 * Every tab opened from another becomes a child branch; searches become roots.
 * Off by default: when disabled, NO ancestry data is recorded at all (zero cost).
 *
 * When enabled, it records HistoryNode {id,url,title,parentId,children,openedAt,
 * closedAt,spaceId,searchQuery} per run-session to `history-trees/<sessionId>.soratree`,
 * and the UI renders the graph (d3-hierarchy + d3-force). Recording starts from the
 * next session; no backfill.
 */
export class HistoryTree implements FeatureModule {
  readonly id = 'feature.historyTree'
  readonly name = 'History Tree'
  readonly description = 'A visual map of how your tabs branched from one another this session.'
  readonly experimental = false
  readonly settingKey = 'historyTreeEnabled' as const

  private unsub: (() => void) | null = null

  activate(ctx: FeatureContext): void {
    // TODO: begin recording ancestry to a per-session .soratree file.
    this.unsub = ctx.onNavigation((_e: NavigationEvent) => {
      // record node + parent linkage
    })
  }

  deactivate(): void {
    this.unsub?.()
    this.unsub = null
  }
}
