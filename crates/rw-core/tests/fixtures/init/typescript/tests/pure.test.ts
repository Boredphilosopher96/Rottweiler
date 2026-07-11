import { resolveInput } from '../packages/engine/src/navigation/resolveInput.ts'
import { importNetscape } from '../packages/engine/src/bookmarks/importNetscape.ts'
import { mergeLWW } from '../packages/engine/src/sync/merge.ts'
import { resolveFilename, sanitizeFilename, isDangerous, splitName } from '../packages/engine/src/downloads/paths.ts'
import { zoomStep } from '../packages/engine/src/contract/zoom.ts'
import { errorPageHtml, errorSummary } from '../packages/engine/src/navigation/errorPage.ts'
import { suggest } from '../packages/engine/src/omnibox/suggest.ts'
import { detectOSFrom, classifyFamily } from '../packages/engine/src/platform/detectOS.ts'
import { savePageFilename, electronSaveType } from '../packages/engine/src/downloads/savePage.ts'
import { classifySecurity } from '../packages/engine/src/navigation/security.ts'
import { extractArticle, scoreBlock, linkDensity } from '../packages/engine/src/reader/readability.ts'
import { PageCacheStore } from '../packages/engine/src/experimental/pageCacheStore.ts'
import { buildAuthPromptBase } from '../packages/engine/src/permissions/authPrompt.ts'
import { PromptQueue } from '../packages/engine/src/permissions/PromptQueue.ts'
import { WindowRegistry } from '../packages/engine/src/windows/WindowRegistry.ts'
import { parseMatchPattern, matchesPattern, matchesAny } from '../packages/engine/src/extensions/matchPattern.ts'
import { parseChromeManifest } from '../packages/engine/src/extensions/chromeManifest.ts'
import { permissionWarnings } from '../packages/engine/src/extensions/permissionWarnings.ts'
import { createVault, unlockVault, changePassword, rotateMasterKey, sealCredential, openCredential, scryptKdf } from '../packages/engine/src/crypto/vaultCrypto.ts'
import { matchEntriesForOrigin, originOf, registrableDomain } from '../packages/engine/src/passwords/autofill.ts'
import { classifyPage, decideZoning } from '../packages/engine/src/safeZoning/policy.ts'

let pass = 0
let fail = 0
function eq(actual: unknown, expected: unknown, label: string): void {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a === e) { pass++ }
  else { fail++; console.log(`  FAIL ${label}\n    expected ${e}\n    got      ${a}`) }
}

// ── resolveInput ──
eq(resolveInput('localhost:8000'), 'http://localhost:8000', 'localhost:port → http')
eq(resolveInput('localhost'), 'http://localhost', 'localhost → http')
eq(resolveInput('127.0.0.1:5173/app'), 'http://127.0.0.1:5173/app', 'ipv4:port/path → http')
eq(resolveInput('192.168.1.10'), 'http://192.168.1.10', 'lan ip → http')
eq(resolveInput('dev.local'), 'http://dev.local', '.local → http')
eq(resolveInput('myserver:8080'), 'http://myserver:8080', 'bare name:port → http')
eq(resolveInput('example.com'), 'https://example.com', 'domain → https')
eq(resolveInput('example.com:8443/x'), 'https://example.com:8443/x', 'domain:port/path → https')
eq(resolveInput('https://already.com/x'), 'https://already.com/x', 'full url passthrough')
eq(resolveInput('file:///home/abi/x.html'), 'file:///home/abi/x.html', 'file scheme passthrough')
eq(resolveInput('about:blank'), 'about:blank', 'about scheme passthrough')
eq(resolveInput('how do i center a div'), 'https://duckduckgo.com/?q=how%20do%20i%20center%20a%20div', 'text → search')
eq(resolveInput('rust'), 'https://duckduckgo.com/?q=rust', 'single word → search')
eq(resolveInput('  '), 'about:blank', 'empty → about:blank')
eq(resolveInput('open ai', 'https://www.google.com/search?q=%s'), 'https://www.google.com/search?q=open%20ai', 'custom search engine')

// ── importNetscape ──
const nsHtml = `<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
  <DT><A HREF="https://a.com" ADD_DATE="1">Alpha &amp; Co</A>
  <DT><H3>Dev</H3>
  <DL><p>
    <DT><A HREF="https://github.com">GitHub</A>
    <DT><A HREF="https://news.ycombinator.com">HN</A>
  </DL><p>
  <DT><A HREF="https://z.com">Zed</A>
</DL><p>`
const nodes = importNetscape(nsHtml, 'ROOT')
const bookmarks = nodes.filter(n => n.type === 'bookmark')
const folders = nodes.filter(n => n.type === 'folder')
eq(bookmarks.length, 4, 'import: 4 bookmarks')
eq(folders.length, 1, 'import: 1 folder')
eq(bookmarks.find(b => b.title === 'Alpha & Co')?.parentId, 'ROOT', 'top bookmark parented to root')
eq(bookmarks.find(b => b.title === 'Alpha & Co')?.url, 'https://a.com', 'entity decoded + url kept')
const devFolder = folders[0]
eq(bookmarks.find(b => b.title === 'GitHub')?.parentId, devFolder.id, 'nested bookmark parented to folder')
eq(bookmarks.find(b => b.title === 'Zed')?.parentId, 'ROOT', 'bookmark after folder close back at root')

// ── mergeLWW ──
const snap = (t: number, id = 'p'): any => ({ profileId: id, schema: 1, updatedAt: t, bookmarks: [], settings: {} })
eq(mergeLWW(snap(10), snap(20)).apply, 'remote', 'newer remote wins')
eq(mergeLWW(snap(30), snap(20)).apply, 'push-local', 'newer local pushes')
eq(mergeLWW(snap(10), snap(10)).apply, 'none', 'equal → none')
eq(mergeLWW(null, snap(5)).apply, 'remote', 'no local → take remote')
eq(mergeLWW(snap(5), null).apply, 'push-local', 'no remote → push local')
eq(mergeLWW(null, null).apply, 'none', 'both null → none')


// -- download filename logic --
const has = (set: Set<string>) => (n: string): boolean => set.has(n)
eq(resolveFilename('a.txt', 'increment', has(new Set())), 'a.txt', 'no collision returns base')
eq(resolveFilename('a.txt', 'increment', has(new Set(['a.txt']))), 'a-01.txt', 'increment first collision → -01')
eq(resolveFilename('a.txt', 'increment', has(new Set(['a.txt', 'a-01.txt']))), 'a-02.txt', 'increment second → -02')
eq(resolveFilename('report.pdf', 'timestamp', has(new Set(['report.pdf'])), new Date(2026, 0, 1, 9, 48, 12)), 'report-09-48-12.pdf', 'timestamp format HH-MM-SS')
eq(resolveFilename('a.txt', 'overwrite', has(new Set(['a.txt']))), 'a.txt', 'overwrite keeps base name')
eq(resolveFilename('noext', 'increment', has(new Set(['noext']))), 'noext-01', 'increment with no extension')
eq(splitName('a.tar.gz').ext, '.gz', 'splitName takes last ext')
eq(sanitizeFilename('a/b:c*?.txt'), 'a_b_c__.txt', 'sanitize strips path/reserved chars')
eq(sanitizeFilename('   '), 'download', 'blank name falls back to download')
eq(isDangerous('setup.exe'), true, 'exe flagged dangerous')
eq(isDangerous('photo.png'), false, 'png not dangerous')


// -- zoom stepping --
eq(zoomStep(1, 1), 1.1, 'zoom in from 100% -> 110%')
eq(zoomStep(1, -1), 0.9, 'zoom out from 100% -> 90%')
eq(zoomStep(3, 1), 3, 'zoom in clamps at max')
eq(zoomStep(0.5, -1), 0.5, 'zoom out clamps at min')
eq(zoomStep(1.23, 1), 1.5, 'zoom snaps to nearest stop then steps')

// -- error page --
const ep = errorPageHtml(-105, 'net::ERR_NAME_NOT_RESOLVED', 'https://nope.example')
eq(ep.includes('https://nope.example'), true, 'error page shows the failed url')
eq(ep.includes('error -105'), true, 'error page shows the code')
eq(errorSummary(-106), 'No internet connection', 'error summary maps offline code')


// -- omnibox suggestions --
const now = Date.now()
const sug = suggest({
  query: 'exam',
  history: [{ url: 'https://example.com', title: 'Example Domain', visitCount: 5, lastVisit: now }],
  bookmarks: [{ title: 'Examples', url: 'https://examples.org' }],
  tabs: [{ title: 'Exam Tab', url: 'https://exam.tab' }],
  now
})
eq(sug[0].kind, 'bookmark', 'bookmark outranks a low-visit history hit')
eq(sug.some((x) => x.kind === 'history' && x.url === 'https://example.com'), true, 'history suggestion included')
eq(sug.some((x) => x.kind === 'tab'), true, 'open-tab suggestion included')
const dedup = suggest({
  query: '',
  history: [{ url: 'https://a.com', title: 'A', visitCount: 9, lastVisit: now }],
  bookmarks: [{ title: 'A bookmark', url: 'https://a.com' }],
  tabs: [],
  now
})
eq(dedup.length, 1, 'suggestions dedup by url')
eq(dedup[0].kind, 'bookmark', 'dedup keeps the higher-scored source')

// ── OS detection ──
const winInfo = detectOSFrom({ platform: 'win32', arch: 'x64', release: '10.0.22631', type: 'Windows_NT' })
eq(winInfo.family, 'windows', 'win32 → windows')
eq(winInfo.isWindows, true, 'win32 isWindows')
eq(winInfo.isMac || winInfo.isLinux || winInfo.isBSD, false, 'win32 exclusive family')
eq(detectOSFrom({ platform: 'darwin', arch: 'arm64', release: '23.5.0', type: 'Darwin' }).family, 'macos', 'darwin → macos')
eq(detectOSFrom({ platform: 'darwin', arch: 'arm64', release: '23.5.0', type: 'Darwin' }).arch, 'arm64', 'darwin arm64 arch')
eq(detectOSFrom({ platform: 'linux', arch: 'x64', release: '6.8.0', type: 'Linux' }).family, 'linux', 'linux → linux')
eq(detectOSFrom({ platform: 'freebsd', arch: 'x64', release: '14.0', type: 'FreeBSD' }).family, 'bsd', 'freebsd → bsd')
eq(detectOSFrom({ platform: 'openbsd', arch: 'x64', release: '7.5', type: 'OpenBSD' }).variant, 'openbsd', 'openbsd variant')
eq(detectOSFrom({ platform: 'netbsd', arch: 'x64', release: '10.0', type: 'NetBSD' }).family, 'bsd', 'netbsd → bsd')
eq(detectOSFrom({ platform: 'dragonfly', arch: 'x64', release: '6.4', type: 'DragonFly' }).variant, 'dragonfly', 'dragonfly variant')
eq(detectOSFrom({ platform: 'android', arch: 'arm64', release: '5.15-android13', type: 'Linux' }).family, 'linux', 'android → linux family')
eq(detectOSFrom({ platform: 'android', arch: 'arm64', release: '5.15-android13', type: 'Linux' }).variant, 'android', 'android variant')
eq(detectOSFrom({ platform: 'linux', arch: 'x64', release: '5.15.0-microsoft-standard-WSL2', type: 'Linux' }).variant, 'wsl', 'wsl variant')
eq(detectOSFrom({ platform: 'sunos', arch: 'x64', release: '5.11', type: 'SunOS' }).family, 'unknown', 'sunos → unknown')
eq(classifyFamily('cygwin', 'windows_nt'), 'windows', 'cygwin → windows')
eq(detectOSFrom({ platform: 'weird', arch: 'x64', release: '1', type: 'Some-BSD-Kernel' }).family, 'bsd', 'kernel-string bsd fallback')

// ── PageCacheStore (SORA-X instant back/forward core) ──
{
  const c = new PageCacheStore<string>(3)
  c.put('t1', 0, 'a', 'A'); c.put('t1', 1, 'b', 'B'); c.put('t1', 2, 'c', 'C')
  eq(c.size(), 3, 'store fills to cap')
  const ev = c.put('t1', 3, 'd', 'D')       // over cap → evict LRU (index 0)
  eq(ev.map((e) => e.value), ['A'], 'cap eviction removes LRU')
  eq(c.has('t1', 0), false, 'evicted key gone')
  c.get('t1', 1)                            // touch B → now MRU
  const ev2 = c.put('t1', 4, 'e', 'E')      // evict LRU which is now C (index 2)
  eq(ev2.map((e) => e.value), ['C'], 'LRU respects recent get()')
  eq(c.take('t1', 1)?.value, 'B', 'take returns value')
  eq(c.has('t1', 1), false, 'take removes entry')
  const closed = c.evictTab('t1')
  eq(c.size(), 0, 'evictTab clears the tab')
  eq(closed.length >= 1, true, 'evictTab returns entries')
}
{
  const c0 = new PageCacheStore<string>(0)     // cap 0 → never retains
  const ev = c0.put('t', 0, 'u', 'V')
  eq(c0.size(), 0, 'cap 0 retains nothing')
  eq(ev.map((e) => e.value), ['V'], 'cap 0 returns value straight back for disposal')
}
{
  // replacing the same key returns the old value for teardown
  const c = new PageCacheStore<string>(3)
  c.put('t', 0, 'u', 'OLD')
  const ev = c.put('t', 0, 'u2', 'NEW')
  eq(ev.map((e) => e.value), ['OLD'], 'replacing a key evicts the old value')
  eq(c.size(), 1, 'replacement keeps one entry')
}


// ── save-page filename ──
eq(savePageFilename('Hello World', 'https://x.com/a', 'html'), 'Hello World.html', 'title → html name')
eq(savePageFilename('Hello World', 'https://x.com/a', 'mhtml'), 'Hello World.mhtml', 'title → mhtml name')
eq(savePageFilename('a/b:c*d', 'https://x.com', 'html'), 'a b c d.html', 'illegal chars scrubbed')
eq(savePageFilename('', 'https://news.example.com/2026/story-title/', 'html'), 'story-title.html', 'empty title → url path stem')
eq(savePageFilename('', 'https://www.example.com/', 'html'), 'example.com.html', 'empty title + root → host stem')
eq(savePageFilename('   ', 'not a url', 'html'), 'page.html', 'blank title + bad url → page')
eq(electronSaveType('mhtml'), 'MHTML', 'mhtml saveType')
eq(electronSaveType('html'), 'HTMLComplete', 'html saveType')
eq(electronSaveType('html-only'), 'HTMLOnly', 'html-only saveType')

// ── security classifier ──
eq(classifySecurity({ url: 'https://example.com/x' }).level, 'secure', 'https → secure')
eq(classifySecurity({ url: 'https://example.com', mixedContent: true }).level, 'mixed', 'https + mixed → mixed')
eq(classifySecurity({ url: 'http://example.com' }).level, 'insecure', 'http → insecure')
eq(classifySecurity({ url: 'https://bad.com', certError: 'ERR_CERT_DATE_INVALID' }).level, 'dangerous', 'cert error → dangerous')
eq(classifySecurity({ url: 'sora://settings' }).level, 'internal', 'sora scheme → internal')
eq(classifySecurity({ url: 'about:blank' }).level, 'internal', 'about → internal')
eq(classifySecurity({ url: 'https://example.com/x' }).encrypted, true, 'https encrypted flag')
eq(classifySecurity({ url: 'http://example.com/x' }).host, 'example.com', 'host parsed')
eq(classifySecurity({ url: 'garbage' }).level, 'unknown', 'unparseable → unknown')
eq(classifySecurity({ url: 'data:text/html,<h1>hi there</h1>' }).level, 'internal', 'messy data: url → internal via scheme fallback')

// ── reader-mode scorer ──
eq(linkDensity({ tag: 'p', text: '0123456789', linkText: '01234', depth: 3 }), 0.5, 'link density 50%')
{
  const navBlock = { tag: 'nav', text: 'Home About Contact Blog', linkText: 'Home About Contact Blog', depth: 2, role: 'navigation' }
  const proseBlock = { tag: 'p', text: 'The quick brown fox jumps over the lazy dog, again and again, with commas and clauses that read like genuine prose, long enough to clear the reader threshold comfortably and then some.', linkText: '', depth: 4 }
  eq(scoreBlock(proseBlock) > scoreBlock(navBlock), true, 'prose scores above nav')
  const art = extractArticle({ title: 'My Article', byline: 'By A. Writer', blocks: [navBlock, proseBlock] })
  eq(art.ok, true, 'article extracted')
  eq(art.blocks.length, 1, 'only prose block chosen')
  eq(art.blocks[0].tag, 'p', 'chosen block is the paragraph')
  eq(art.byline, 'By A. Writer', 'byline captured')
}
{
  const onlyNav = extractArticle({ title: 'X', blocks: [{ tag: 'nav', text: 'A B C', linkText: 'A B C', depth: 2 }] })
  eq(onlyNav.ok, false, 'no article → ok false')
}

// ── http basic auth: prompt mapper ──
{
  const base = buildAuthPromptBase({ realm: 'Secure Area', host: 'example.com:8080', isProxy: false }, 'https://example.com:8080/private')
  eq(base.kind, 'httpAuth', 'auth prompt kind')
  eq(base.realm, 'Secure Area', 'auth realm carried')
  eq(base.host, 'example.com:8080', 'auth host carried')
  eq(base.origin, 'https://example.com:8080', 'auth origin from url')
  eq(base.isProxy, false, 'auth isProxy flag')
  eq(buildAuthPromptBase({ isProxy: true }, 'http://proxy.local/').isProxy, true, 'proxy auth flagged')
}

// ── http basic auth: queue credential flow ──
{
  const q = new PromptQueue()
  let got: unknown = 'unset'
  const id = q.addAuth({ kind: 'httpAuth', origin: 'https://x.com', host: 'x.com', realm: 'R' }, (creds) => { got = creds })
  eq(q.list().length, 1, 'auth prompt queued')
  eq(q.list()[0].kind, 'httpAuth', 'queued prompt is httpAuth')
  q.respondAuth(id, { username: 'neo', password: 'trinity' })
  eq(got, { username: 'neo', password: 'trinity' }, 'respondAuth delivers credentials')
  eq(q.list().length, 0, 'answered auth prompt removed')
}
{
  const q = new PromptQueue()
  let got: unknown = 'unset'
  const id = q.addAuth({ kind: 'httpAuth', origin: 'https://x.com' }, (creds) => { got = creds })
  q.respond(id, false, false)                 // cancel via the generic respond path
  eq(got, null, 'cancelling an auth prompt resolves null')
}

// ── multi-window registry ──
{
  const r = new WindowRegistry()
  r.open('w1', 1000); r.open('w2', 1001)
  eq(r.count(), 2, 'two windows open')
  eq(r.focused(), 'w2', 'newest window is focused')
  eq(r.get('w1')?.focused, false, 'older window not focused')
  r.focus('w1')
  eq(r.focused(), 'w1', 'focus switches')
  r.setTabCount('w1', 5)
  eq(r.get('w1')?.tabCount, 5, 'tab count tracked')
  r.open('w3', 1002)          // w3 focused
  r.close('w3')               // focus returns to most-recently-focused (w1)
  eq(r.focused(), 'w1', 'closing focused window refocuses MRU')
  eq(r.count(), 2, 'window removed on close')
  r.close('w2'); 
  eq(r.isLast(), true, 'one window left → isLast')
  r.close('w1')
  eq(r.focused(), null, 'no windows → no focus')
  eq(r.count(), 0, 'all closed')
}

// ── chrome match patterns ──
eq(matchesPattern('*://*.example.com/*', 'https://www.example.com/x'), true, 'subdomain wildcard matches')
eq(matchesPattern('*://*.example.com/*', 'https://example.com/'), true, 'bare domain matches *.domain')
eq(matchesPattern('*://*.example.com/*', 'https://evil.com/'), false, 'other domain does not match')
eq(matchesPattern('*://*.example.com/*', 'ftp://www.example.com/'), false, '* scheme is http/https only')
eq(matchesPattern('https://example.com/foo/*', 'https://example.com/foo/bar'), true, 'path glob matches')
eq(matchesPattern('https://example.com/foo/*', 'https://example.com/bar'), false, 'path glob rejects other path')
eq(matchesPattern('<all_urls>', 'https://anything.dev/x'), true, '<all_urls> matches https')
eq(matchesPattern('<all_urls>', 'file:///etc/hosts'), true, '<all_urls> matches file')
eq(parseMatchPattern('http://*foo.com/*'), null, 'invalid mid-host wildcard rejected')
eq(parseMatchPattern('not-a-pattern'), null, 'garbage pattern rejected')
eq(matchesAny(['*://*.a.com/*', '*://*.b.com/*'], 'https://x.b.com/y'), true, 'matchesAny hits second pattern')

// ── chrome manifest parsing ──
{
  const mv3 = parseChromeManifest({
    manifest_version: 3, name: 'Test', version: '1.2.0', description: 'd',
    permissions: ['tabs', 'storage'], host_permissions: ['https://*.example.com/*'],
    action: { default_title: 'Go', default_popup: 'popup.html' },
    background: { service_worker: 'bg.js' },
    content_scripts: [{ matches: ['*://*/*'], js: ['cs.js'], run_at: 'document_end' }],
    icons: { '16': 'i16.png' }
  })
  eq(mv3.ok, true, 'valid mv3 manifest ok')
  eq(mv3.manifest?.manifestVersion, 3, 'mv3 detected')
  eq(mv3.manifest?.permissions.join(','), 'tabs,storage', 'api permissions kept')
  eq(mv3.manifest?.hostPermissions.join(','), 'https://*.example.com/*', 'host permissions captured')
  eq(mv3.manifest?.background, 'service_worker', 'mv3 background is service worker')
  eq(mv3.manifest?.action.kind, 'action', 'mv3 action kind')
  eq(mv3.manifest?.action.defaultPopup, 'popup.html', 'action popup captured')
  eq(mv3.manifest?.contentScripts[0].runAt, 'document_end', 'content script run_at')
}
{
  // MV2 folds host patterns into permissions and uses browser_action + background.scripts
  const mv2 = parseChromeManifest({
    manifest_version: 2, name: 'Legacy', version: '0.9',
    permissions: ['tabs', '<all_urls>', 'https://api.foo.com/*'],
    browser_action: { default_title: 'L' },
    background: { scripts: ['b.js'] }
  })
  eq(mv2.manifest?.manifestVersion, 2, 'mv2 detected')
  eq(mv2.manifest?.permissions.join(','), 'tabs', 'mv2 host-like perms split out')
  eq(mv2.manifest?.hostPermissions.includes('<all_urls>'), true, 'mv2 all_urls moved to host perms')
  eq(mv2.manifest?.background, 'scripts', 'mv2 background scripts')
  eq(mv2.manifest?.action.kind, 'browser_action', 'mv2 browser_action')
}
{
  const bad = parseChromeManifest({ name: 'NoVersion' })
  eq(bad.ok, false, 'manifest missing version/mv is not ok')
  eq(bad.errors.length > 0, true, 'errors reported')
}

// ── permission warnings ──
{
  const m = parseChromeManifest({ manifest_version: 3, name: 'W', version: '1',
    permissions: ['tabs', 'clipboardRead', 'storage'], host_permissions: ['<all_urls>'] }).manifest!
  const warns = permissionWarnings(m)
  eq(warns[0].severity, 'high', 'most severe warning first')
  eq(warns.some((w) => w.permission === 'host:all'), true, 'all-urls host warning present')
  eq(warns.some((w) => w.permission === 'tabs'), true, 'tabs warning present')
  eq(warns.some((w) => w.permission === 'storage'), false, 'benign storage not warned')
}
{
  const m = parseChromeManifest({ manifest_version: 3, name: 'H', version: '1',
    host_permissions: ['https://*.github.com/*', 'https://*.gitlab.com/*'] }).manifest!
  const warns = permissionWarnings(m)
  eq(warns.some((w) => w.permission === 'host:some' && /github\.com/.test(w.message)), true, 'specific-host warning lists domains')
}

// ── password vault: envelope encryption ──
await (async () => {
  const kdf = scryptKdf
  const v = await createVault('correct horse battery staple', kdf, 'scrypt')
  eq(v.masterKey.length, 32, 'master key is 256-bit')

  // entry round-trips under the master key
  const sealedCred = sealCredential(v.masterKey, { username: 'neo', password: 's3cr3t', notes: 'zion' })
  const back = openCredential(v.masterKey, sealedCred)
  eq(back.username === 'neo' && back.password === 's3cr3t' && back.notes === 'zion', true, 'credential seals + opens')

  // unlock with the right password recovers the SAME master key
  const mk2 = await unlockVault(v.header, 'correct horse battery staple', kdf)
  eq(Buffer.from(mk2).equals(Buffer.from(v.masterKey)), true, 'unlock recovers master key')

  // wrong password throws
  let threw = false
  try { await unlockVault(v.header, 'wrong', kdf) } catch { threw = true }
  eq(threw, true, 'wrong password rejected')

  // change password: entries (sealed under master key) stay valid, master key unchanged
  const newHeader = await changePassword(v.header, 'correct horse battery staple', 'new-passphrase-2026', kdf)
  const mk3 = await unlockVault(newHeader, 'new-passphrase-2026', kdf)
  eq(Buffer.from(mk3).equals(Buffer.from(v.masterKey)), true, 'password change keeps master key (entries untouched)')
  const stillReadable = openCredential(mk3, sealedCred)
  eq(stillReadable.password, 's3cr3t', 'entry still decrypts after password change')
  let oldPwFails = false
  try { await unlockVault(newHeader, 'correct horse battery staple', kdf) } catch { oldPwFails = true }
  eq(oldPwFails, true, 'old password no longer unlocks after change')

  // rotate master key: entries re-encrypted, old sealed blob no longer opens under new master
  const entries = [{ id: 'e1', secret: sealedCred }]
  const rot = await rotateMasterKey(v.header, 'correct horse battery staple', entries, kdf)
  const newMaster = await unlockVault(rot.header, 'correct horse battery staple', kdf)
  eq(Buffer.from(newMaster).equals(Buffer.from(v.masterKey)), false, 'rotation changes the master key')
  const resealed = rot.resealed.get('e1')
  eq(!!resealed, true, 'rotation reseals each entry')
  eq(openCredential(newMaster, resealed).password, 's3cr3t', 'resealed entry opens under new master key')
  let oldSealFails = false
  try { openCredential(newMaster, sealedCred) } catch { oldSealFails = true }
  eq(oldSealFails, true, 'pre-rotation blob will not open under the new master key')
})()

// ── autofill origin matching ──
eq(originOf('https://example.com/login?x=1'), 'https://example.com', 'originOf strips path')
eq(originOf('ftp://example.com/'), null, 'originOf rejects non-web scheme')
eq(registrableDomain('login.accounts.example.com'), 'example.com', 'registrable domain heuristic')
{
  const now = Date.now()
  const entries = [
    { id: 'a', origin: 'https://example.com', label: 'example.com', secret: { iv: '', ct: '', tag: '' }, createdAt: 0, updatedAt: now - 100 },
    { id: 'b', origin: 'https://login.example.com', label: 'login.example.com', secret: { iv: '', ct: '', tag: '' }, createdAt: 0, updatedAt: now },
    { id: 'c', origin: 'https://evil.com', label: 'evil.com', secret: { iv: '', ct: '', tag: '' }, createdAt: 0, updatedAt: now }
  ]
  const m = matchEntriesForOrigin(entries, 'https://example.com/login')
  eq(m[0].id, 'a', 'exact-origin match ranks first')
  eq(m.some((e) => e.id === 'b'), true, 'same registrable-domain match included')
  eq(m.some((e) => e.id === 'c'), false, 'unrelated domain excluded')
}

// ── SafeZoning policy ──
{
  const login = { url: 'https://example.com/login', hasPasswordField: true, hasLoginForm: true, crossOriginForm: false }
  const benign = { url: 'https://example.com/blog/post', hasPasswordField: false, hasLoginForm: false, crossOriginForm: false }
  eq(classifyPage(login).sensitive, true, 'login page classified sensitive')
  eq(classifyPage(login).score >= 50, true, 'login page scores high')
  eq(classifyPage(benign).sensitive, false, 'blog page not sensitive')
  eq(decideZoning('off', login).action, 'allow', 'off mode never acts')
  eq(decideZoning('basic', login).action, 'harden', 'basic hardens sensitive pages')
  eq(decideZoning('basic', benign).action, 'allow', 'basic leaves benign pages alone')
  eq(decideZoning('forced', login).action, 'isolate', 'forced isolates sensitive pages')
  eq(decideZoning('forced', benign).action, 'harden', 'forced hardens everything')
  eq(classifyPage({ url: 'https://accounts.google.com/', hasPasswordField: false, hasLoginForm: false, crossOriginForm: false }).reasons.some((r) => /hostname/.test(r)), true, 'auth-like hostname is a reason')
  eq(classifyPage({ url: 'https://shop.com/pay', hasPasswordField: false, hasLoginForm: false, crossOriginForm: true }).reasons.some((r) => /cross-origin/.test(r)), true, 'cross-origin form flagged')
}


console.log(`\n${fail === 0 ? 'ALL PASS' : 'FAILURES'}: ${pass} passed, ${fail} failed`)
process.exit(fail === 0 ? 0 : 1)
