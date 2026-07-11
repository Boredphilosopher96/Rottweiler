// How a host app configures the engine. Keeps @sora/engine UI-agnostic: the app
// supplies its own chrome (preload + entry), so other projects can build a
// different browser on the same base.

export interface BrowserChromeConfig {
  /** absolute path to the chrome (UI) preload script. */
  preloadPath: string
  /** dev server URL for the chrome UI (takes precedence if set). */
  devUrl?: string
  /** built HTML entry for the chrome UI. */
  fileEntry?: string
}

export interface BrowserWindowConfig {
  width?: number
  height?: number
  minWidth?: number
  minHeight?: number
  backgroundColor?: string
}

export interface BrowserConfig {
  chrome: BrowserChromeConfig
  window?: BrowserWindowConfig
}
