import type { ChromeManifest, PermissionWarning } from '../contract/chromeExtensions'

const KNOWN: Record<string, { message: string; severity: PermissionWarning['severity'] }> = {
  tabs: { message: 'Read your browsing history', severity: 'medium' },
  history: { message: 'Read and change your browsing history', severity: 'high' },
  cookies: { message: 'Read and change cookies for sites it can access', severity: 'medium' },
  webRequest: { message: 'Observe and analyze your network traffic', severity: 'high' },
  webRequestBlocking: { message: 'Block or modify your network requests', severity: 'high' },
  declarativeNetRequest: { message: 'Block or modify content on pages', severity: 'medium' },
  bookmarks: { message: 'Read and change your bookmarks', severity: 'medium' },
  downloads: { message: 'Manage your downloads', severity: 'medium' },
  geolocation: { message: 'Detect your physical location', severity: 'high' },
  clipboardRead: { message: 'Read data you copy and paste', severity: 'high' },
  clipboardWrite: { message: 'Modify data you copy and paste', severity: 'low' },
  nativeMessaging: { message: 'Communicate with cooperating native applications', severity: 'high' },
  management: { message: 'Manage your apps, extensions, and themes', severity: 'high' },
  proxy: { message: 'Control how you connect to the internet', severity: 'high' },
  privacy: { message: 'Change your privacy-related settings', severity: 'high' },
  debugger: { message: 'Access the page debugger backend', severity: 'high' },
  notifications: { message: 'Display notifications', severity: 'low' },
  contextMenus: { message: 'Add items to the right-click menu', severity: 'low' }
}

function hostWarning(hosts: string[]): PermissionWarning | null {
  if (hosts.length === 0) return null
  if (hosts.includes('<all_urls>') || hosts.some((h) => /^\*:\/\/\*\//.test(h) || h === '*://*/*'))
    return { permission: 'host:all', message: 'Read and change all your data on all websites you visit', severity: 'high' }
  const domains = hosts.map((h) => { try { return new URL(h.replace('*://', 'https://')).hostname.replace(/^\*\./, '') } catch { return h } })
  const uniq = [...new Set(domains)].filter(Boolean)
  const list = uniq.slice(0, 3).join(', ') + (uniq.length > 3 ? `, +${uniq.length - 3} more` : '')
  return { permission: 'host:some', message: `Read and change your data on ${list}`, severity: 'medium' }
}

/** Produce the install-time warnings a manifest would trigger, most severe first. */
export function permissionWarnings(manifest: ChromeManifest): PermissionWarning[] {
  const out: PermissionWarning[] = []
  const host = hostWarning(manifest.hostPermissions)
  if (host) out.push(host)
  for (const p of manifest.permissions) {
    const k = KNOWN[p]
    if (k) out.push({ permission: p, message: k.message, severity: k.severity })
  }
  const rank = { high: 0, medium: 1, low: 2 }
  return out.sort((a, b) => rank[a.severity] - rank[b.severity])
}
