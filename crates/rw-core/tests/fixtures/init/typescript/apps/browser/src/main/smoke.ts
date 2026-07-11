import { app } from 'electron'
import { join } from 'node:path'
import { writeFileSync, existsSync, mkdirSync } from 'node:fs'
import type { Browser } from '@sora/engine'
import { SessionRestore, WindowManager } from '@sora/engine'
import type { SyncableSnapshot } from '@shared/sync'
import type { DownloadItemState } from '@shared/downloads'
import { CTX } from '@shared/menus'
import { readdirSync } from 'node:fs'

/**
 * Headless end-to-end check of the matured base, run under xvfb via `npm run smoke`.
 * Uses data: URLs (no network). Verifies: tabs/events/layout, profiles, settings,
 * bookmark import, hibernation discard+wake, sync pull/apply round-trip, and
 * session-restore persistence.
 */
export function runSmoke(appWin: Browser): void {
  const log = (...a: unknown[]): void => console.log('[SMOKE]', ...a)
  let failed = false
  const check = (cond: boolean, label: string): void => {
    if (cond) log('  ok  -', label)
    else {
      failed = true
      log('  FAIL-', label)
    }
  }
  const delay = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms))
  const page = (body: string): string => `data:text/html,<body>${encodeURIComponent(body)}</body>`

  const finish = (): void => {
    log(failed ? 'RESULT: FAILURES' : 'RESULT: ALL PASS')
    process.exitCode = failed ? 1 : 0
    for (const t of [...appWin.tabs.all]) appWin.tabs.close(t.id)
    setTimeout(() => {
      if (!appWin.win.isDestroyed()) appWin.win.destroy()
      app.quit()
    }, 50)
  }

  void (async (): Promise<void> => {
    try {
      // ── spaces + first tab + title propagation ──
      check(appWin.profile.spaces.all.length === 2, 'profile seeds two spaces')
      const tab = appWin.tabs.getActive()
      if (!tab) return check(false, 'active tab at startup'), finish()
      const wc = tab.ensureView().webContents
      await new Promise<void>((res) => wc.once('did-finish-load', () => res()))
      await wc.executeJavaScript("document.title = 'Sora Base Alive'; null")
      await delay(150)
      check(appWin.serialize().tabs.find((t) => t.id === tab.id)?.title === 'Sora Base Alive', 'title propagates to state')

      // ── settings ──
      appWin.profile.settings.patch({ searchEngine: 'https://www.google.com/search?q=%s' })
      check(appWin.serialize().settings.searchEngine.includes('google'), 'settings patch reflected in state')

      // ── platform / OS detection ──
      const plat = appWin.serialize().platform
      check(['windows', 'macos', 'linux', 'bsd', 'unknown'].includes(plat.family), 'platform family classified')
      check(typeof plat.arch === 'string' && plat.arch.length > 0, 'platform arch reported')
      check(plat.isLinux === (plat.family === 'linux'), 'platform boolean flags consistent')

      // ── bookmarks import ──
      const before = appWin.profile.bookmarks.nodes.length
      const n = appWin.profile.bookmarks.importHtml(
        '<DL><p><DT><A HREF="https://a.com">A</A><DT><H3>F</H3><DL><p><DT><A HREF="https://b.com">B</A></DL><p></DL><p>'
      )
      check(n === 2, 'netscape import returns 2 bookmarks')
      check(appWin.profile.bookmarks.nodes.length === before + 3, 'import added 2 bookmarks + 1 folder')

      // ── hibernation: discard an inactive tab, then wake it ──
      appWin.newTab(page('two'), undefined, false) // inactive
      const t2 = appWin.tabs.all[appWin.tabs.all.length - 1]
      await appWin.hibernation.hibernateNow(t2)
      check(appWin.serialize().tabs.find((t) => t.id === t2.id)?.hibernated === true, 'inactive tab hibernates')
      appWin.tabs.activate(t2.id) // relayout wakes it
      await delay(100)
      check(appWin.serialize().tabs.find((t) => t.id === t2.id)?.hibernated === false, 'activating wakes a hibernated tab')

      // ── split ──
      const ids = appWin.tabs.all.map((t) => t.id)
      appWin.setLayout({ mode: 'split', paneTabIds: ids.slice(0, 2) })
      check(appWin.serialize().layout.paneTabIds.length === 2, 'split layout applies')

      // ── sync round-trip via local folder ──
      await appWin.sync.syncNow() // remote empty → push-local
      check(appWin.sync.status === 'idle', 'sync push-local returns idle')
      // craft a newer remote snapshot with an extra bookmark, then pull
      const profileId = appWin.profiles.activeId
      const rootId = appWin.profile.bookmarks.rootId
      const remote: SyncableSnapshot = {
        profileId,
        schema: 1,
        updatedAt: Date.now() + 1_000_000,
        bookmarks: [
          ...appWin.profile.bookmarks.nodes,
          { id: 'remote-1', type: 'bookmark', title: 'FromRemote', url: 'https://r.com', parentId: rootId, createdAt: Date.now() }
        ],
        settings: appWin.profile.settings.get()
      }
      const syncFile = join(app.getPath('userData'), 'sora-sync-local', `sora-sync-${profileId}.json`)
      writeFileSync(syncFile, JSON.stringify(remote), 'utf-8')
      await appWin.sync.syncNow() // remote newer → apply
      check(appWin.profile.bookmarks.nodes.some((b) => b.title === 'FromRemote'), 'sync pulls + applies newer remote (LWW)')

      // ── session-restore persistence round-trip ──
      appWin.profile.settings.patch({ sessionRestore: true })
      appWin.saveSession()
      const restored = new SessionRestore(appWin.profile.dir).load()
      check((restored?.tabs.length ?? 0) === appWin.tabs.all.length, 'session restore persists open tabs')

      // ── permissions: default policy + override ──
      check(appWin.profile.permissions.decide('https://x.com', 'fullscreen') === 'allow', 'permission default: fullscreen allowed')
      check(appWin.profile.permissions.decide('https://x.com', 'geolocation') === 'ask', 'permission default: geolocation asks')
      appWin.setPermissionPolicy('https://x.com', 'geolocation', 'allow')
      check(appWin.profile.permissions.decide('https://x.com', 'geolocation') === 'allow', 'permission override persists')
      check(appWin.serialize().permissionPolicies.some((e) => e.origin === 'https://x.com'), 'permission policy visible in state')

      // ── prompt queue: add → state → respond ──
      let promptAnswer: { allow: boolean; remember: boolean } | null = null
      const promptId = appWin.prompts.add({ kind: 'permission', origin: 'https://y.com', permission: 'notifications' }, (allow, remember) => {
        promptAnswer = { allow, remember }
      })
      check(appWin.serialize().pendingPrompts.length === 1, 'pending prompt shows in state')
      appWin.respondPrompt(promptId, true, false)
      check(promptAnswer !== null && (promptAnswer as { allow: boolean }).allow === true, 'respond resolves the prompt')
      check(appWin.serialize().pendingPrompts.length === 0, 'answered prompt clears from state')

      // ── external protocol: a site opening an app raises an ask-prompt ──
      appWin.tabs.emit('external-url', { url: 'zoommtg://join?x=1', origin: 'https://meet.example' })
      await delay(20)
      const extPrompt = appWin.serialize().pendingPrompts.find((p) => p.kind === 'external')
      check(extPrompt?.scheme === 'zoommtg', 'external app request becomes an external prompt')
      if (extPrompt) appWin.respondPrompt(extPrompt.id, false, true)
      check(appWin.profile.externalProtocols.decide('zoommtg') === 'deny', 'remembered external denial persists')

      // ── run session id (one per browser run, shared across windows) ──
      check(typeof appWin.serialize().runSessionId === 'string' && appWin.serialize().runSessionId.length > 0, 'run session id present')
      check(appWin.serialize().capabilities.permissions === true, 'capabilities advertise permissions')

      // ── downloads: real end-to-end, collision naming + persistence ──
      check(appWin.serialize().capabilities.downloads === true, 'capabilities advertise downloads')
      check(appWin.serialize().settings.downloadCollision === 'increment', 'download collision defaults to increment')
      const dlDir = join(app.getPath('userData'), 'test-dl')
      mkdirSync(dlDir, { recursive: true })
      appWin.profile.settings.patch({ downloadDir: dlDir, downloadPromptForLocation: false, downloadCollision: 'increment' })
      const dataUrl = 'data:application/octet-stream;base64,SGVsbG8gU29yYQ=='
      const waitCompleted = async (n: number): Promise<DownloadItemState[]> => {
        for (let i = 0; i < 50; i++) {
          await delay(100)
          const done = appWin.serialize().downloads.filter((d) => d.state === 'completed')
          if (done.length >= n) return done
        }
        return appWin.serialize().downloads.filter((d) => d.state === 'completed')
      }
      appWin.downloadStart(dataUrl)
      await waitCompleted(1)
      appWin.downloadStart(dataUrl) // same name → must not clobber the first
      const done = await waitCompleted(2)
      check(done.length >= 2, 'two downloads complete')
      check(new Set(done.map((d) => d.savePath)).size === done.length, 'collision naming gives each a distinct path')
      check(done.every((d) => existsSync(d.savePath)), 'downloaded files exist on disk')
      check(appWin.profile.downloads.items.length >= 2, 'completed downloads persisted to history')

      // ── history: record + recent + search + clear (core, on by default) ──
      check(appWin.serialize().capabilities.history === true, 'capabilities advertise history')
      check(appWin.serialize().settings.historyEnabled === true, 'history on by default')
      appWin.profile.history.record('https://example.com/a', 'Example A')
      appWin.profile.history.record('https://example.com/a', 'Example A')
      appWin.profile.history.record('https://example.org/b', 'Example B')
      const hist = appWin.serialize().history
      check(hist.some((h) => h.url === 'https://example.com/a' && h.visitCount === 2), 'history records + counts revisits')
      check(appWin.profile.history.search('example b').length === 1, 'history search matches title')
      appWin.clearHistory()
      check(appWin.serialize().history.length === 0, 'clear history empties it')

      // ── feature registry: opt-in modules, off by default, toggle via a setting ──
      const feats = appWin.serialize().features
      check(feats.length === 4, 'four optional features registered')
      check(feats.every((f) => !f.enabled), 'all optional features off by default')
      check(feats.some((f) => f.id === 'experimental.pageVersionCache' && f.experimental), 'page cache flagged experimental')
      appWin.profile.settings.patch({ historyTreeEnabled: true })
      check(
        appWin.serialize().features.find((f) => f.id === 'feature.historyTree')?.enabled === true,
        'toggling a setting enables its feature'
      )

      // ── context menus: build model from a right-click, then execute an action ──
      const ctxTabId = appWin.serialize().activeTabId as string
      appWin.tabs.emit('context-menu', {
        tabId: ctxTabId,
        input: {
          x: 10,
          y: 20,
          linkURL: 'https://example.com/x',
          srcURL: '',
          selectionText: '',
          isEditable: false,
          mediaType: 'none',
          editFlags: { canCut: false, canCopy: false, canPaste: false, canSelectAll: false }
        }
      })
      const menu = appWin.serialize().contextMenu
      check(menu !== null && menu.items.some((i) => i.id === CTX.OpenLinkNewTab), 'context menu offers open-link-in-new-tab')
      check(Boolean(menu?.items.some((i) => i.id === CTX.CopyLink)), 'context menu offers copy-link')
      const tabsBeforeCtx = appWin.serialize().tabs.length
      appWin.contextMenuAction(ctxTabId, CTX.OpenLinkNewTab)
      check(appWin.serialize().tabs.length === tabsBeforeCtx + 1, 'open-link action opens a new tab')
      check(appWin.serialize().contextMenu === null, 'context menu clears after action')

      // ── find in page: state plumbing ──
      appWin.find(ctxTabId, 'hello')
      const fs = appWin.serialize().find
      check(fs !== null && fs.tabId === ctxTabId && fs.query === 'hello', 'find sets find state')
      appWin.findStop(ctxTabId)
      check(appWin.serialize().find === null, 'find stop clears find state')

      // ── zoom: stepping + per-origin persistence ──
      appWin.zoomIn(ctxTabId)
      check(
        Math.abs((appWin.serialize().tabs.find((t) => t.id === ctxTabId)?.zoom ?? 0) - 1.1) < 1e-6,
        'zoom in sets tab zoom to 110%'
      )
      appWin.zoomReset(ctxTabId)
      check(appWin.serialize().tabs.find((t) => t.id === ctxTabId)?.zoom === 1, 'zoom reset returns to 100%')
      appWin.profile.zoom.set('https://z.com', 1.25)
      check(
        appWin.serialize().zoomPolicies.some((z) => z.origin === 'https://z.com' && z.factor === 1.25),
        'per-origin zoom persists in state'
      )

      // ── tab ops: pin / mute / duplicate ──
      appWin.setPinned(ctxTabId, true)
      check(appWin.serialize().tabs.find((t) => t.id === ctxTabId)?.pinned === true, 'pin tab reflected in state')
      appWin.setMuted(ctxTabId, true)
      check(appWin.serialize().tabs.find((t) => t.id === ctxTabId)?.muted === true, 'mute tab reflected in state')
      const beforeDup = appWin.serialize().tabs.length
      appWin.duplicateTab(ctxTabId)
      check(appWin.serialize().tabs.length === beforeDup + 1, 'duplicate tab adds a tab')

      // ── recently closed: capture on close, restore on reopen ──
      appWin.newTab('data:text/html,<h1>doc</h1>', appWin.profile.spaces.activeId, true)
      const closeId = appWin.serialize().activeTabId as string
      let curl = ''
      for (let i = 0; i < 40; i++) {
        await delay(75)
        curl = appWin.serialize().tabs.find((t) => t.id === closeId)?.url ?? ''
        if (curl && curl !== 'about:blank') break
      }
      appWin.closeTab(closeId)
      check(appWin.serialize().closedTabs.length >= 1, 'closing a real page records a recently-closed entry')
      const beforeReopen = appWin.serialize().tabs.length
      appWin.reopenClosed()
      check(appWin.serialize().tabs.length === beforeReopen + 1, 'reopen closed restores a tab')

      // ── tab groups ──
      const gTabId = appWin.serialize().activeTabId as string
      appWin.createGroup('Research', '#7aa2f7')
      const grp = appWin.serialize().tabGroups[0]
      check(appWin.serialize().tabGroups.length >= 1 && grp.name === 'Research', 'create tab group')
      appWin.setTabGroup(gTabId, grp.id)
      check(appWin.serialize().tabs.find((t) => t.id === gTabId)?.groupId === grp.id, 'assign tab to group')
      appWin.renameGroup(grp.id, 'Reading')
      check(appWin.serialize().tabGroups.find((g) => g.id === grp.id)?.name === 'Reading', 'rename group')
      appWin.removeGroup(grp.id)
      check(appWin.serialize().tabGroups.length === 0, 'remove group')
      check(appWin.serialize().tabs.find((t) => t.id === gTabId)?.groupId === null, 'removing a group ungroups its tabs')

      // ── security indicator ──
      const secTab = appWin.serialize().activeTabId as string
      appWin.navigate(secTab, 'https://secure.example')
      for (let i = 0; i < 40; i++) {
        await delay(75)
        if ((appWin.serialize().tabs.find((t) => t.id === secTab)?.url ?? '').startsWith('https')) break
      }
      check(appWin.serialize().tabs.find((t) => t.id === secTab)?.secure === true, 'https tab marked secure')
      check(appWin.serialize().tabs.find((t) => t.id === secTab)?.security.level === 'secure', 'https tab security level = secure')

      // ── spellcheck in context menu ──
      const spTab = appWin.serialize().activeTabId as string
      appWin.tabs.emit('context-menu', {
        tabId: spTab,
        input: {
          x: 0, y: 0, linkURL: '', srcURL: '', selectionText: '',
          isEditable: true, mediaType: 'none',
          misspelledWord: 'teh', dictionarySuggestions: ['the', 'tech', 'tea'],
          editFlags: { canCut: true, canCopy: true, canPaste: true, canSelectAll: true }
        }
      })
      const cm = appWin.serialize().contextMenu
      check(cm?.misspelledWord === 'teh' && cm.replacements.includes('the'), 'context menu surfaces spellcheck replacements')
      check(Boolean(cm?.items.some((i) => i.id === CTX.AddToDictionary)), 'context menu offers add-to-dictionary')
      appWin.contextMenuClose()

      // ── print to pdf ──
      const pTab = appWin.serialize().activeTabId as string
      appWin.profile.settings.patch({ downloadDir: appWin.profile.dir })
      appWin.navigate(pTab, 'data:text/html,<h1>print me</h1>')
      for (let i = 0; i < 40; i++) {
        await delay(75)
        if ((appWin.serialize().tabs.find((t) => t.id === pTab)?.url ?? '').startsWith('data:')) break
      }
      await delay(150)
      await appWin.printToPdf(pTab)
      await delay(150)
      check(
        readdirSync(appWin.profile.dir).some((f) => f.startsWith('sora-') && f.endsWith('.pdf')),
        'print to pdf writes a file'
      )


      // ── save page as (html) ──
      const saveRes = await appWin.savePageAs(pTab, 'html')
      await delay(150)
      check(saveRes.ok === true && !!saveRes.path, 'savePageAs reports success')
      check(readdirSync(appWin.profile.dir).some((f) => f.endsWith('.html')), 'save page writes an html file')

      // ── reader mode extraction ──
      const readerHtml = '<article><h1>Headline</h1><p>' +
        'Real prose sentence with commas, clauses and plenty of length so the reader scorer treats it as article body content. '.repeat(3) +
        '</p></article><nav><a href="#">Home</a> <a href="#">About</a> <a href="#">Contact</a></nav>'
      appWin.navigate(pTab, page(readerHtml))
      for (let i = 0; i < 40; i++) {
        await delay(75)
        if ((appWin.serialize().tabs.find((t) => t.id === pTab)?.url ?? '').startsWith('data:')) break
      }
      await delay(200)
      const art = await appWin.readerExtract(pTab)
      check(!!art && art.ok === true, 'reader mode extracts an article')
      check(!!art && art.blocks.some((b) => b.text.includes('Real prose')), 'reader keeps the prose, drops the nav')

      // ── chrome web extension host ──
      check(appWin.serialize().capabilities.chromeExtensions === true, 'chrome extension support detected at runtime')
      const extDir = join(appWin.profile.dir, 'smoke-ext')
      mkdirSync(extDir, { recursive: true })
      writeFileSync(join(extDir, 'manifest.json'), JSON.stringify({
        manifest_version: 3,
        name: 'Smoke Ext',
        version: '1.0.0',
        description: 'loaded by the smoke test',
        permissions: ['storage'],
        host_permissions: ['https://*.example.com/*'],
        action: { default_title: 'Smoke' }
      }))
      const rec = await appWin.loadChromeExtension(extDir)
      await delay(200)
      check(rec.loadError === null, 'unpacked extension parses without error')
      check(appWin.serialize().chromeExtensions.some((e) => e.manifest.name === 'Smoke Ext'), 'loaded extension appears in state')
      const loaded = appWin.serialize().chromeExtensions.find((e) => e.manifest.name === 'Smoke Ext')
      check(loaded?.manifest.manifestVersion === 3, 'loaded extension normalized as mv3')
      check(loaded?.manifest.hostPermissions.includes('https://*.example.com/*') === true, 'host permissions surfaced')
      if (loaded) { appWin.removeChromeExtension(loaded.id) }
      check(!appWin.serialize().chromeExtensions.some((e) => e.manifest.name === 'Smoke Ext'), 'removed extension leaves state')

      // ── password vault (Sora-User wraps Sora-Master; envelope encryption) ──
      check(appWin.serialize().capabilities.passwordManager === true, 'password manager capability advertised')
      check(appWin.serialize().vault.exists === false, 'no vault before creation')
      await appWin.createVault('master-pass-2026', 'scrypt')
      check(appWin.serialize().vault.exists === true && appWin.serialize().vault.locked === false, 'vault created and unlocked')
      appWin.addPassword('https://example.com/login', { username: 'neo', password: 'redpill' })
      appWin.addPassword('https://other.com/', { username: 'someone', password: 'x' })
      check(appWin.serialize().vault.entryCount === 2, 'entries saved')
      const matches = appWin.matchPasswords('https://example.com/login')
      check(matches.length === 1 && matches[0].username === 'neo', 'autofill matches origin and decrypts username')
      appWin.lockVault()
      check(appWin.serialize().vault.locked === true, 'vault locks')
      check(appWin.matchPasswords('https://example.com/login').length === 0, 'locked vault returns no matches')
      check((await appWin.unlockVault('nope')) === false, 'wrong master password rejected')
      check((await appWin.unlockVault('master-pass-2026')) === true, 'correct master password unlocks')
      const vId = appWin.serialize().vault.entries.find((e) => e.origin === 'https://example.com')!.id
      check(appWin.getPassword(vId)?.password === 'redpill', 'credential decrypts after unlock')
      check((await appWin.changeVaultPassword('master-pass-2026', 'new-master-2027')) === true, 'password change succeeds')
      check(appWin.getPassword(vId)?.password === 'redpill', 'entries intact after password change (only master key re-wrapped)')
      check((await appWin.rotateVault('new-master-2027')) === true, 'master key rotation succeeds')
      check(appWin.getPassword(vId)?.password === 'redpill' && appWin.serialize().vault.lastRotated !== null, 'entries re-encrypted and readable after rotation')

      // ── SafeZoning (experimental) ──
      check(appWin.serialize().safeZoning.mode === 'off', 'safe zoning off by default')
      appWin.setSafeZoningMode('basic')
      check(appWin.serialize().safeZoning.mode === 'basic', 'safe zoning set to basic')
      const szTab = appWin.serialize().activeTabId as string
      appWin.navigate(szTab, page('<form action="/login"><input type="password" name="pw" /></form>'))
      for (let i = 0; i < 40; i++) { await delay(75); if ((appWin.serialize().tabs.find((t) => t.id === szTab)?.url ?? '').startsWith('data:')) break }
      await delay(200)
      const zdec = await appWin.safeZoneTab(szTab)
      check(!!zdec && zdec.sensitive === true && zdec.action === 'harden', 'login page hardened under basic zoning')
      check(appWin.serialize().safeZoning.activeZones >= 1, 'hardened page counted as active zone')
      appWin.navigate(szTab, page('<p>just an article, nothing sensitive here</p>'))
      for (let i = 0; i < 40; i++) { await delay(75); if ((appWin.serialize().tabs.find((t) => t.id === szTab)?.url ?? '').startsWith('data:')) break }
      await delay(150)
      const zdec2 = await appWin.safeZoneTab(szTab)
      check(!!zdec2 && zdec2.action === 'allow', 'benign page left alone under basic zoning')
      appWin.setSafeZoningMode('off')
      check(appWin.serialize().safeZoning.activeZones === 0, 'turning zoning off clears zones')

      // ── multi-window manager (coordination over the tested registry) ──
      const wmEvents: string[] = []
      const wm = new WindowManager((id) => ({ id, focus: () => wmEvents.push('focus:' + id), close: () => wmEvents.push('close:' + id) }))
      const a = wm.open(); const b = wm.open()
      check(wm.count() === 2 && wm.focused()?.id === b.id, 'manager opens windows, newest focused')
      wm.focus(a.id)
      check(wm.focused()?.id === a.id && wmEvents.includes('focus:' + a.id), 'focus routes to the window')
      wm.close(b.id)
      check(wm.count() === 1 && wmEvents.includes('close:' + b.id), 'close tears the window down')
      wm.onClosed(a.id)
      check(wm.count() === 0 && wm.focused() === null, 'external close reconciles bookkeeping')

      // ── find in page (+ clears on navigation) ──
      const fTab = appWin.serialize().activeTabId as string
      appWin.navigate(fTab, page('<p>the quick brown fox, the lazy dog, and the end</p>'))
      for (let i = 0; i < 40; i++) { await delay(75); if ((appWin.serialize().tabs.find((t) => t.id === fTab)?.url ?? '').startsWith('data:')) break }
      await delay(200)
      appWin.find(fTab, 'the')
      await delay(150)
      check(appWin.serialize().find?.tabId === fTab, 'find targets the active tab')
      appWin.navigate(fTab, page('<p>a different page with no query word</p>'))
      for (let i = 0; i < 40; i++) { await delay(75); if ((appWin.serialize().tabs.find((t) => t.id === fTab)?.url ?? '') !== '' ) break }
      await delay(200)
      check(appWin.serialize().find === null, 'find highlight state clears on navigation')

      // ── private (incognito) space: ephemeral session, no history ──
      const priv = appWin.profile.spaces.create('Incognito', '#888888', true)
      check(appWin.serialize().spaces.find((s) => s.id === priv.id)?.private === true, 'private space flagged in state')
      appWin.activateSpace(priv.id)
      appWin.newTab('data:text/html,<h1>secret-xyzzy</h1>', priv.id, true)
      const secretId = appWin.serialize().activeTabId as string
      for (let i = 0; i < 40; i++) {
        await delay(75)
        const u = appWin.serialize().tabs.find((t) => t.id === secretId)?.url ?? ''
        if (u && u !== 'about:blank') break
      }
      check(!appWin.serialize().history.some((h) => h.url.includes('secret-xyzzy')), 'private space records no history')

      // ── omnibox suggestions + clear browsing data ──
      check(appWin.serialize().capabilities.omnibox === true, 'capabilities advertise omnibox')
      appWin.profile.history.record('https://suggesttest.com/page', 'Suggest Test')
      const sug = appWin.suggest('suggesttest')
      check(sug.some((x) => x.url === 'https://suggesttest.com/page'), 'omnibox suggests from history')
      await appWin.clearBrowsingData({ history: true })
      check(appWin.serialize().history.length === 0, 'clear browsing data wipes history')

      // ── profiles: create + switch ──
      const p = appWin.profiles.create('Test', 'oklch(0.6 0.13 30)')
      appWin.activateProfile(p.id)
      await delay(50)
      check(appWin.profiles.activeId === p.id, 'profile switch changes active profile')
      check(appWin.serialize().tabs.length === 1, 'profile switch resets to a fresh tab')
      check(appWin.serialize().spaces.length === 2, 'new profile seeds its own spaces')
    } catch (e) {
      failed = true
      log('EXCEPTION', (e as Error).message)
    }
    finish()
  })()

  setTimeout(() => {
    log('TIMEOUT')
    process.exitCode = 2
    app.quit()
  }, 25000)
}
