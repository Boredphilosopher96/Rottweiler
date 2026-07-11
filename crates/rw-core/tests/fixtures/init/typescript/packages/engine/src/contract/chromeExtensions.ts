// Chrome web extension ABI (page-content extensions), distinct from Sora's own
// native GUI extensions in ./extensions.ts. These describe loaded .crx/unpacked
// extensions surfaced to the UI (management page, action toolbar, permissions).

export type ManifestVersion = 2 | 3
export type BackgroundKind = 'service_worker' | 'scripts' | 'page' | 'none'

export interface ContentScriptDef {
  matches: string[]
  excludeMatches: string[]
  js: string[]
  css: string[]
  runAt: 'document_start' | 'document_end' | 'document_idle'
  allFrames: boolean
}

export interface ChromeActionDef {
  kind: 'action' | 'browser_action' | 'page_action' | 'none'
  defaultTitle: string
  defaultPopup: string | null
  defaultIcon: Record<string, string> | string | null
}

/** Normalized manifest, independent of MV2/MV3 shape differences. */
export interface ChromeManifest {
  name: string
  version: string
  manifestVersion: ManifestVersion
  description: string
  permissions: string[]
  optionalPermissions: string[]
  hostPermissions: string[]
  contentScripts: ContentScriptDef[]
  background: BackgroundKind
  action: ChromeActionDef
  icons: Record<string, string>
  optionsPage: string | null
}

export interface ChromeExtensionRecord {
  id: string
  manifest: ChromeManifest
  path: string
  enabled: boolean
  source: 'unpacked' | 'crx'
  loadError: string | null
}

export interface PermissionWarning {
  permission: string
  message: string
  severity: 'high' | 'medium' | 'low'
}
