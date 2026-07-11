# Sora ⚈ ᴥ ⚈

A workflow-first Chromium browser on Electron. The point isn't to be big, it's to get the
basics genuinely right and deliver the actual workflows people use, with extras opt-in and
off by default. No baked-in AI, no VPN, no crypto. Built on a hardened, reusable engine so
the UI can be built later against something solid.

This is a monorepo:

- **`packages/engine`** ([`@sora/engine`](packages/engine)), a reusable, UI-agnostic Chromium
  browser base. Everything below sits behind one typed, versioned contract, so another project
  could build a different browser on it. The engine never imports the renderer; all UI ↔ engine
  traffic goes through the contract, and the UI holds no authoritative state (it renders
  `BrowserState` snapshots).
- **`apps/browser`** (`@sora/browser`), the Sora product: the engine, Sora's config, and (later)
  the PaperDesign chrome. Today the renderer is just a dev harness (˘ω˘) not the real UI yet.

## Develop

```
npm install
npm run dev         # launch
npm run test        # 184 pure unit tests
npm run typecheck   # engine + app
npm run smoke       # 108 headless engine checks (xvfb)
npm run build
npm run verify      # typecheck + test + build
```

## Status: v0.1.0 base (⁄ ⁄•⁄ω⁄•⁄ ⁄)

The base is grown deliberately, tested-core-first, before the UI is started. Working and tested,
so far (◕‿◕✿):

**Core browsing**: tabs, URL/search resolution, profiles, isolated Spaces, split view, tab
hibernation, session restore, bookmarks + Netscape import, visit history (record/search/clear),
context menus, find-in-page (with match-case + clear-on-navigation), per-origin zoom,
recently-closed tabs, tab pin/mute/duplicate, themed error pages, frecency omnibox suggestions,
clear-browsing-data, tab groups, private (incognito) Spaces.

**Security & privacy**: per-site permissions, external-protocol ("open in app") handling,
per-tab security posture (secure / mixed / insecure / dangerous / internal) with cert-error
tracking, HTTP basic-auth prompts, and **SafeZoning** (experimental): Sora-side hardening of
sensitive pages (`basic` injects a content-injection guard; `forced` isolation is scaffolded).

**Password vault**: envelope encryption (`user password → Argon2id → KEK → wraps → Master Key
→ encrypts → entries`). Changing the password re-wraps only the master key; master-key rotation
(opt-in) re-encrypts everything. AES-256-GCM, Argon2id via hash-wasm, origin-matched autofill ( •̀ ω •́ )✧

**Content & platform**: reader mode (readability scorer + in-page extraction), save page as
(HTML/MHTML), Chrome web-extension host (loads unpacked MV2/MV3, permission warnings, match
patterns; capability is runtime-detected: `basic` on Electron 33+, `full` on 35+), OS detection
(Windows / macOS / Linux / BSD + variants) for adaptive UX, and a multi-window manager.

**Extensibility**: a versioned ABI, a native (GUI-contributing) extension foundation, and an
opt-in feature registry hosting History Tree / Ghost Tab / Reading Position Sync / experimental
PageVersionCache (instant back/forward) modules.

Every capability is advertised through a runtime-detected `Capabilities` object; the contract
version is `1.0` and all additions so far have been additive (˶ˆᗜˆ˵)

## Architecture invariants ( ⚈ ⚈ )ﾉ

- The engine (`packages/engine`) is UI-agnostic and never imports renderer code.
- All UI ↔ engine communication is via the typed contract in `packages/engine/src/contract`.
- The UI holds no authoritative state, it just renders `BrowserState` snapshots.
- Pure logic (crypto envelope, match patterns, readability, security classification, KDF, LRU,
  policy, etc.) lives in dependency-light modules unit-tested by `tests/pure.test.ts`; Electron
  wiring is exercised by the headless smoke.

GPL-3.0-or-later, MilkmanAbi. ( ˊᵕˋ )♡
