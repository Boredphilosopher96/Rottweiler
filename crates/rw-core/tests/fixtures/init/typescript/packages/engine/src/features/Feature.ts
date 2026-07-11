import type { SoraSettings } from '@shared/settings'
import type { FeatureInfo } from '@shared/features'

/** Navigation fact handed to interested features. */
export interface NavigationEvent {
  url: string
  title: string
  tabId: string
  spaceId: string
}

/** What a feature module is given to do its job. Kept small on purpose. */
export interface FeatureContext {
  profileDir(): string
  onNavigation(cb: (e: NavigationEvent) => void): () => void
  emitChange(): void
}

/** An optional/experimental capability that plugs into the engine, gated by a setting. */
export interface FeatureModule {
  readonly id: string
  readonly name: string
  readonly description: string
  readonly experimental: boolean
  readonly settingKey: keyof SoraSettings
  activate(ctx: FeatureContext): void
  deactivate(): void
}

/**
 * Hosts optional features. `sync(settings)` activates newly-enabled modules and
 * deactivates newly-disabled ones, so toggling a setting toggles the feature.
 * Everything here is opt-in; nothing runs unless its setting is on.
 */
export class FeatureRegistry {
  private activeIds = new Set<string>()

  constructor(
    private readonly modules: FeatureModule[],
    private readonly ctx: FeatureContext
  ) {}

  list(settings: SoraSettings): FeatureInfo[] {
    return this.modules.map((m) => ({
      id: m.id,
      name: m.name,
      description: m.description,
      experimental: m.experimental,
      enabled: Boolean(settings[m.settingKey])
    }))
  }

  sync(settings: SoraSettings): void {
    for (const m of this.modules) {
      const want = Boolean(settings[m.settingKey])
      const on = this.activeIds.has(m.id)
      if (want && !on) {
        m.activate(this.ctx)
        this.activeIds.add(m.id)
      } else if (!want && on) {
        m.deactivate()
        this.activeIds.delete(m.id)
      }
    }
  }
}
