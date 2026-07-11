// @sora/engine — a reusable, UI-agnostic Chromium browser engine on Electron.
// A host app supplies chrome (preload + entry) via BrowserConfig; the engine owns
// tabs, sessions, profiles, downloads, permissions, sync, extensions and the rest.

export { Browser } from './core/Browser'
export { registerIpc } from './ipc/registerIpc'
export { SessionRestore } from './session-restore/SessionRestore'
export { WindowManager, type ManagedWindow } from './windows/WindowManager'
export { WindowRegistry } from './windows/WindowRegistry'

// the full versioned contract (types + channels + defaults) for host apps
export * from './contract/config'
export * from './contract/version'
export * from './contract/types'
export * from './contract/ipc'
export * from './contract/settings'
export * from './contract/bookmarks'
export * from './contract/sync'
export * from './contract/extensions'
export * from './contract/permissions'
export * from './contract/downloads'
export * from './contract/menus'
export * from './contract/find'
export * from './contract/zoom'
export * from './contract/omnibox'
