// The contribution ABI for Sora's own native (GUI-modifying) extensions.
// Distinct from Chrome web extensions (page content), which come later via electron-chrome-extensions.

export interface SidebarContribution {
  id: string
  label: string
  icon?: string
  /** command id fired when clicked. */
  command: string
}

export interface CommandContribution {
  id: string
  title: string
}

export interface ThemeContribution {
  id: string
  name: string
  /** css variable overrides, e.g. { '--accent': 'oklch(...)' } */
  vars: Record<string, string>
}

export interface Contributions {
  sidebarItems?: SidebarContribution[]
  commands?: CommandContribution[]
  themes?: ThemeContribution[]
}

export interface SoraExtensionManifest {
  id: string
  name: string
  version: string
  description?: string
  contributes?: Contributions
}

export interface ExtensionRecord {
  manifest: SoraExtensionManifest
  enabled: boolean
}
