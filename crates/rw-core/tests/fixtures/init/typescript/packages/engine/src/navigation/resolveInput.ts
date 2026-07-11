// Turn raw address-bar text into a loadable URL, or a search query URL.
// Modeled on real-browser behavior: local hosts and IPs go to http, public
// hostnames to https, everything non-URL-shaped becomes a search.

const NO_AUTHORITY_SCHEMES = /^(about|data|blob|chrome|view-source|mailto|javascript|sora):/i
const HAS_AUTHORITY_SCHEME = /^[a-z][a-z0-9+.-]*:\/\//i

const IPV4 = /^\d{1,3}(\.\d{1,3}){3}$/
const LOCAL_TLD = /\.(local|localhost|test|internal)$/i

function isLocalHostPart(host: string): boolean {
  const h = host.toLowerCase()
  return (
    h === 'localhost' ||
    h === '::1' ||
    h === '[::1]' ||
    IPV4.test(h) ||
    LOCAL_TLD.test(h)
  )
}

export function resolveInput(input: string, searchTemplate = 'https://duckduckgo.com/?q=%s'): string {
  const s = input.trim()
  if (!s) return 'about:blank'

  // already a full URL, or a scheme that takes no authority
  if (HAS_AUTHORITY_SCHEME.test(s) || NO_AUTHORITY_SCHEMES.test(s)) return s

  if (!s.includes(' ')) {
    // split off path/query, keep host[:port]
    const slash = s.indexOf('/')
    const authority = slash === -1 ? s : s.slice(0, slash)
    const host = authority.split(':')[0]
    const port = authority.includes(':') ? authority.split(':')[1] : ''
    const portIsNumeric = port === '' || /^\d+$/.test(port)

    if (portIsNumeric) {
      // local host / IP → http (dev servers, LAN)
      if (isLocalHostPart(host)) return 'http://' + s
      // bare "name:port" with no dot → treat as a local dev host on http
      if (port !== '' && !host.includes('.')) return 'http://' + s
      // public domain shape "something.tld[...]" → https
      if (/^[^.\s]+(\.[^.\s]+)+$/.test(host)) return 'https://' + s
    }
  }

  return searchTemplate.replace('%s', encodeURIComponent(s))
}
