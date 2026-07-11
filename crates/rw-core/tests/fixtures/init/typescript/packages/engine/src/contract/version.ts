// The stable ABI version. Bump MAJOR for breaking contract-shape changes (the UI
// shell must then adapt); bump MINOR for additive capabilities (old shells keep working).
export const CONTRACT_VERSION = { major: 1, minor: 0 } as const

/** Feature flags the engine advertises. The UI reads these and only shows what exists,
 *  so the engine can gain features without breaking an older shell. */
export interface Capabilities {
  tabs: boolean
  spaces: boolean
  profiles: boolean
  split: boolean
  hibernation: boolean
  bookmarks: boolean
  settings: boolean
  sync: boolean
  nativeExtensions: boolean
  chromeExtensions: boolean // runtime-detected: 'basic' via loadExtension (E33+), 'full' via session.extensions (E35+)
  sessionRestore: boolean
  permissions: boolean
  externalProtocols: boolean
  downloads: boolean
  history: boolean
  contextMenus: boolean
  findInPage: boolean
  zoom: boolean
  omnibox: boolean
  tabGroups: boolean
  privateSpaces: boolean
  spellcheck: boolean
  printToPdf: boolean
  savePage: boolean
  readerMode: boolean
  passwordManager: boolean
}

export interface Hello {
  version: typeof CONTRACT_VERSION
  capabilities: Capabilities
}
