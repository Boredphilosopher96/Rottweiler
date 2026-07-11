import type { CollisionStrategy } from './downloads'

export type TabPolicy = 'awake' | 'sleep'

export interface SoraSettings {
  /** search URL template; %s is replaced with the encoded query. */
  searchEngine: string
  /** restore last session on launch. Off by default (owner preference; friends can flip it on). */
  sessionRestore: boolean
  /** 'awake' = never sleep tabs, no background throttle (RAM cost, live network);
   *  'sleep' = hibernate inactive tabs past threshold (saves RAM, reload cost). */
  tabPolicy: TabPolicy
  hibernateAfterMinutes: number
  theme: 'system' | 'light' | 'dark'
  accent: string
  /** '' = OS downloads folder. */
  downloadDir: string
  /** ask where to save each file (native dialog) vs save silently to downloadDir. */
  downloadPromptForLocation: boolean
  /** naming when a file already exists: classic increment vs timestamp vs overwrite. */
  downloadCollision: CollisionStrategy
  /** record visit history (core, on). */
  historyEnabled: boolean
  /** opt-in optional/experimental features (privacy: all default off). */
  historyTreeEnabled: boolean
  ghostTabEnabled: boolean
  readingPositionEnabled: boolean
  experimentalPageCache: boolean
}

export const DEFAULT_SETTINGS: SoraSettings = {
  searchEngine: 'https://duckduckgo.com/?q=%s',
  sessionRestore: false,
  tabPolicy: 'sleep',
  hibernateAfterMinutes: 30,
  theme: 'system',
  accent: 'oklch(0.62 0.13 250)',
  downloadDir: '',
  downloadPromptForLocation: false,
  downloadCollision: 'increment',
  historyEnabled: true,
  historyTreeEnabled: false,
  ghostTabEnabled: false,
  readingPositionEnabled: false,
  experimentalPageCache: false
}

/** A few ready-to-use engines so the UI can offer a picker without hardcoding. */
export const SEARCH_ENGINES: Record<string, string> = {
  DuckDuckGo: 'https://duckduckgo.com/?q=%s',
  Google: 'https://www.google.com/search?q=%s',
  Bing: 'https://www.bing.com/search?q=%s',
  Brave: 'https://search.brave.com/search?q=%s',
  Startpage: 'https://www.startpage.com/sp/search?query=%s'
}
