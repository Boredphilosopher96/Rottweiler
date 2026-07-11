import type { ChromeManifest, ContentScriptDef, ChromeActionDef, BackgroundKind, ManifestVersion } from '../contract/chromeExtensions'

// Chrome host-permission-looking entries embedded in MV2 "permissions".
function looksLikeHost(p: string): boolean {
  return p === '<all_urls>' || /:\/\//.test(p) || /^\*:\/\//.test(p)
}

function asStringArray(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x) => typeof x === 'string') as string[] : []
}

function normContentScripts(raw: unknown): ContentScriptDef[] {
  if (!Array.isArray(raw)) return []
  return raw.map((cs) => {
    const o = (cs ?? {}) as Record<string, unknown>
    const runAt = o.run_at === 'document_start' || o.run_at === 'document_end' ? o.run_at : 'document_idle'
    return {
      matches: asStringArray(o.matches),
      excludeMatches: asStringArray(o.exclude_matches),
      js: asStringArray(o.js),
      css: asStringArray(o.css),
      runAt: runAt as ContentScriptDef['runAt'],
      allFrames: Boolean(o.all_frames)
    }
  })
}

function normAction(o: Record<string, unknown>, mv: ManifestVersion): ChromeActionDef {
  const src = (mv === 3 ? o.action : (o.browser_action ?? o.page_action)) as Record<string, unknown> | undefined
  const kind: ChromeActionDef['kind'] = mv === 3
    ? (o.action ? 'action' : 'none')
    : (o.browser_action ? 'browser_action' : o.page_action ? 'page_action' : 'none')
  if (!src) return { kind, defaultTitle: '', defaultPopup: null, defaultIcon: null }
  return {
    kind,
    defaultTitle: typeof src.default_title === 'string' ? src.default_title : '',
    defaultPopup: typeof src.default_popup === 'string' ? src.default_popup : null,
    defaultIcon: (src.default_icon as Record<string, string> | string) ?? null
  }
}

function normBackground(o: Record<string, unknown>): BackgroundKind {
  const bg = o.background as Record<string, unknown> | undefined
  if (!bg) return 'none'
  if (typeof bg.service_worker === 'string') return 'service_worker'
  if (Array.isArray(bg.scripts)) return 'scripts'
  if (typeof bg.page === 'string') return 'page'
  return 'none'
}

export interface ManifestParseResult {
  ok: boolean
  manifest: ChromeManifest | null
  errors: string[]
}

/** Parse a raw manifest.json object into Sora's normalized ChromeManifest. */
export function parseChromeManifest(raw: unknown): ManifestParseResult {
  const errors: string[] = []
  if (!raw || typeof raw !== 'object') return { ok: false, manifest: null, errors: ['manifest is not an object'] }
  const o = raw as Record<string, unknown>

  const mvRaw = o.manifest_version
  const manifestVersion: ManifestVersion = mvRaw === 2 ? 2 : mvRaw === 3 ? 3 : 3
  if (mvRaw !== 2 && mvRaw !== 3) errors.push('missing or unsupported manifest_version (expected 2 or 3)')

  const name = typeof o.name === 'string' ? o.name : ''
  const version = typeof o.version === 'string' ? o.version : ''
  if (!name) errors.push('missing name')
  if (!version) errors.push('missing version')

  const rawPerms = asStringArray(o.permissions)
  const permissions = rawPerms.filter((p) => !looksLikeHost(p))
  const hostFromPerms = rawPerms.filter(looksLikeHost)
  const hostPermissions = [...asStringArray(o.host_permissions), ...hostFromPerms]

  const manifest: ChromeManifest = {
    name,
    version,
    manifestVersion,
    description: typeof o.description === 'string' ? o.description : '',
    permissions,
    optionalPermissions: asStringArray(o.optional_permissions),
    hostPermissions,
    contentScripts: normContentScripts(o.content_scripts),
    background: normBackground(o),
    action: normAction(o, manifestVersion),
    icons: (o.icons && typeof o.icons === 'object') ? (o.icons as Record<string, string>) : {},
    optionsPage: typeof o.options_page === 'string' ? o.options_page
      : (o.options_ui && typeof (o.options_ui as Record<string, unknown>).page === 'string'
        ? (o.options_ui as Record<string, string>).page : null)
  }
  return { ok: errors.length === 0, manifest, errors }
}
