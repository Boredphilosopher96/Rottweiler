# @sora/engine

A reusable, UI-agnostic Chromium browser engine on Electron. It owns everything a
browser does below the chrome — tabs, sessions, profiles, downloads, permissions,
external protocols, history, sync, extensions, and an opt-in feature layer — and
exposes it all behind a single typed, versioned contract. A host app supplies its
own chrome (a preload script + a UI entry) via `BrowserConfig`; the engine never
imports UI code. Sora is one product built on it; another project could build a
different browser on the same base.

## Use it

```ts
import { app } from 'electron'
import { Browser, registerIpc, type BrowserConfig } from '@sora/engine'

app.whenReady().then(() => {
  const config: BrowserConfig = {
    chrome: {
      preloadPath: '/abs/path/to/your/preload.js',
      // one of:
      devUrl: process.env.DEV_URL,            // dev server
      fileEntry: '/abs/path/to/your/index.html' // built UI
    }
  }
  const browser = new Browser(config)
  registerIpc(browser)               // wires the typed command channel
})
```

Your chrome talks to the engine over the contract in `src/contract` (also
re-exported from the package root): it receives full `BrowserState` snapshots and
sends `Cmd`s. It holds no authoritative state.

## Public API

`Browser`, `registerIpc`, `SessionRestore`, plus the full contract: `BrowserConfig`,
`BrowserState`, `Cmd` / `CmdPayloads` / `SoraApi` / `Evt`, `Capabilities` /
`CONTRACT_VERSION`, and the domain types (`SoraSettings`, `BookmarkNode`,
`HistoryEntry`, `DownloadItemState`, `PermissionPolicyEntry`, `FeatureInfo`, …).

## Layout

```
src/
  contract/       the versioned ABI: types, ipc channels, capabilities, config, per-domain shapes
  core/Browser.ts the controller (the only class that bridges UI and engine)
  tabs/           Tab, TabManager, LayoutManager (split), HibernationManager (sleep/awake)
  sessions/       partition-per-(profile,space)
  profiles/       ProfileManager + ProfileContext (owns each profile's managers)
  storage/        cache/data clearing
  navigation/     URL-vs-search resolution
  persistence/    tiny JSON Store
  bookmarks/      manager + Netscape import
  history/        HistoryManager (record/recent/search/clear)
  downloads/      DownloadManager (runtime) + DownloadHistoryStore + paths (pure)
  permissions/    PermissionPolicyStore, ExternalPolicyStore, PromptQueue
  sync/           SyncEngine (LWW) + LocalFolder / GoogleDrive adapters + merge
  extensions/     native contribution registry (host lands here)
  features/        FeatureRegistry + FeatureModule + opt-in modules (historyTree, ghostTab, readingPosition)
  experimental/   Sora-experimental modules (PageVersionCache)
  runtime/        run-session id
  ipc/            registerIpc
  index.ts        public API
```

## Invariants

- The engine never imports from a renderer. One-way.
- Everything UI↔engine crosses the typed contract. UI renders snapshots; it is not authoritative.
- Tab content is always sandboxed + contextIsolation + no Node. Chrome is trusted (preload).
- New features add `Capabilities`; a breaking contract shape bumps `CONTRACT_VERSION.major`.
- Optional/experimental features are opt-in and do nothing unless their setting is on.

## Peer requirement

`electron >= 33`. Chrome web-extension support (via electron-chrome-extensions) needs
Electron 35+, a deliberate later bump that flips the `chromeExtensions` capability on.

GPL-3.0-or-later.
