// Pure themed error page. No fs/electron here so it's unit-testable; the Tab wraps
// the returned HTML in a data: URL and loads it on a main-frame load failure.

function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : c === '"' ? '&quot;' : '&#39;'
  )
}

/** Friendly one-liner for the common Chromium net error codes. */
export function errorSummary(code: number): string {
  switch (code) {
    case -105:
      return "This site can't be found"
    case -106:
      return 'No internet connection'
    case -118:
      return 'The connection timed out'
    case -137:
      return "This site's server can't be found"
    case -201:
    case -202:
      return "This site's security certificate isn't trusted"
    case -501:
      return 'This connection is not secure'
    default:
      return "This page didn't load"
  }
}

export function errorPageHtml(code: number, description: string, url: string): string {
  const summary = errorSummary(code)
  return `<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
  :root { color-scheme: dark light }
  html,body { height:100% }
  body { margin:0; display:grid; place-items:center; font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
         background:#111214; color:#e7e7ea }
  .card { max-width:34rem; padding:2.5rem; }
  h1 { font-size:1.5rem; margin:0 0 .5rem }
  p { color:#a8a8b3; margin:.25rem 0 }
  code { color:#c9c9d4; background:#1c1d21; padding:.1rem .35rem; border-radius:.3rem; word-break:break-all }
  .meta { margin-top:1.5rem; font-size:.85rem; color:#6d6d78 }
  button { margin-top:1.75rem; font:inherit; color:#e7e7ea; background:#2a2b30; border:1px solid #3a3b42;
           border-radius:.55rem; padding:.55rem 1.1rem; cursor:pointer }
  button:hover { background:#33343a }
</style></head>
<body><div class="card">
  <h1>${esc(summary)}</h1>
  <p>Sora couldn't load <code>${esc(url)}</code>.</p>
  <p class="meta">${esc(description)} · error ${code}</p>
  <button onclick="location.reload()">Try again</button>
</div></body></html>`
}
