# Field audit — 2026-08-22

Fix checklist from a runtime audit of the shipped Homebrew cask `0.1.4`
(darwin-arm64) and the working tree at `f67909b`, on macOS 26.6.1 / Apple
silicon. Scoped to the three product theses: **extremely fast**, **beautiful
UI**, **pi-grade extensibility**.

**Basis tags** — **[verified]** reproduced or measured at runtime here ·
**[code]** confirmed by reading the source, failure not executed ·
**[open]** a question I did not resolve; the experiment is stated, not the answer.

**Severity** — **P0** breaks or falsely certifies a headline capability ·
**P1** a real bug users will hit · **P2** rough edge.

Effort is my estimate for a scoped Codex brief, not a team-week.

## Resolution — 2026-08-22

The measurement, speed, UI, correctness, packaging, and scaffold defects below
are implemented and covered by executable checks. Two production-plugin phases
remain intentionally open: the public SDK will first be published by an exact-tag
release after npm trusted-publisher configuration, and the accepted TypeScript
source-host/live-attachment design is staged behind its feasibility and migration
gates. Neither is reported as shipped.

| Area | Resolution and evidence |
|---|---|
| Measurement | Replaced the splash-as-paint metric with separate process-start and transcript-painted-plus-keystroke measurements; added a mounted 8 KiB tool-output fixture with real Tree-sitter; every M4 home now contains a representative 4,000-model pricing catalog. |
| Hot paths | Batched tool output at 16 ms/64 KiB, made presentation deferred by default, deferred pricing parsing, moved reducer eviction off delta events, removed the SSE byte-at-a-time/re-encode path, and added a private content-addressed Tree-sitter cache with lazy non-Markdown grammar registration. |
| Release profile | A checked-in 100-sample comparison selected macOS `opt-level = 3`: `z` was 26,546,624 bytes with 32,402 us engine p99 / 103,241 us headless p99; `3` was 36,209,968 bytes in the controlled thin-LTO build with 23,663 us / 95,181 us. The qualified fat-LTO engine is 29,658,880 bytes. See `benchmarks/release-optimization-2026-08-22.json`. |
| UI stability | The isolated OpenTUI harness did not reproduce unbounded growth across 40,000 mounted/destroyed renderables: RSS grew 60,407,808 bytes and plateaued near 122,126,336 bytes (128,237,568 maximum). The recycle remains a safety net and now preserves the draft and scroll offset; lazy grammars reduce idle pressure. |
| Correctness | Incremental UTF-8 carry preserves split multibyte output. Parent-death handling uses Linux `PDEATHSIG`, macOS `kqueue NOTE_EXIT`, and a portable polling fallback; M4 kills the supervisor, proves both children exit, and relaunches the exact workspace without manual recovery. The lease itself is already a kernel `flock`, so there was no stale PID record to reclaim. |
| Distribution | Homebrew now exposes a symlink instead of an environment wrapper, and package-manager ownership is detected from canonical Cellar/Caskroom paths. |
| SDK front door | PR CI installs the exact packed SDK rather than rewriting the dependency to workspace source. The exact-tag release uses npm OIDC trusted publishing, verifies an existing version byte-for-byte on rerun, then installs the unmodified public dependency in a clean scaffold. Actual registry publication awaits the next tag and repository trusted-publisher setup. |
| Plugin architecture | `manifest.json` is the one inert declaration and is validated by `parsePluginManifest`; executing TypeScript to discover authority was rejected. ADR-027 and `docs/design/typescript-source-plugin-host.md` specify one release-owned host process per plugin, two-pass sealing, current-runtime resolution, immutable per-turn registries, and actor-owned live development. The production host and live attachment are designed, not implemented. |
| Baselines | Darwin was recalibrated from the new 100-sample M4/headless rulers and real-renderer TUI fixtures. Linux retains prior measured metrics but is deliberately marked bootstrap for the two new rulers, so protected `--require-measured` qualification stays closed until a native Linux calibration replaces the ceilings. |

`scripts/check-field-audit-remediation.py` fails closed if the remediation shape
regresses. Checked items below are implemented or experimentally settled; open
items name genuine publication or staged-runtime work.

---

## Cross-cutting root cause

Three of the four headline performance metrics measure a synthetic best case,
and `benchmarks/performance-baseline.json` was then frozen from those
measurements. That is why the gates are green while the product is not fast:
**the guarantee and the experience are measuring different things.** Fix the
ruler before optimizing against it, or this recurs in two months with new
numbers.

---

## MSR — Measurement integrity

These are gate defects, not runtime defects. They cost users nothing directly;
they cost you the ability to know anything.

- [x] **MSR-1 · P0 · [verified] · ~1 day** — `tui_first_paint` measures a static
      string, not the application.
      `packages/tui/src/index.ts:283` calls `writeStartupSplash()` then
      `markFirstPaint()` at module top level — before `loadOpenTui()`, the
      renderer, grammar materialization, or the engine connection. Driving the
      shipped binary on a pty: splash at **25.8 ms**, first real OpenTUI output
      at **390.9 ms**, grammars written at **456.6 ms**. The M4 AC
      ("cold start → first paint <150ms", `docs/06-ROADMAP.md`) passes at 109 ms
      against a metric that does not measure the app.
      **Fix:** keep the splash marker as `tui_process_start`; add
      `tui_interactive_p99` — composer accepts a keystroke and the transcript has
      painted. Gate on that.
      **Assessment:** the single most important item in this document. Every
      other speed claim in the project is currently unfalsifiable. Cheap to fix
      and it will immediately turn several other items red, which is the point.

- [x] **MSR-2 · P0 · [verified] · ~1 day** — the frame-budget test exercises the
      one path that is already coalesced.
      `packages/tui/test/perf/performance.test.ts:48` streams assistant text into
      `streamingTail` — deliberately the path the transcript keeps *out* of
      history layout — with `MockTreeSitterClient` returning `{ highlights: [] }`.
      It never touches `tool_output_delta`, `retainRecentTools`, or
      `presentableTranscript`. On that easy path it still reports **17.36 ms p95**
      on macOS, over the 16.7 ms budget for 60 fps.
      **Fix:** second fixture — `tool_output_delta` at realistic 8 KiB pipe
      cadence into a transcript with mounted tool cards, tree-sitter live.
      **Assessment:** pairs with SPD-2; do them in the same round so the fix has a
      number attached. Expect the new fixture to fail badly on first run. That is
      a success condition, not a setback.

- [x] **MSR-3 · P1 · [verified] · ~2 hours** — engine readiness is measured in a
      home directory that has no pricing table.
      `crates/rw-cli/tests/m4_release_gate.py:348` builds a synthetic `$HOME`
      containing only `config.toml`, so `models.toml` is absent,
      `crates/rw-runtime/src/session_host.rs:326` takes the
      `PricingTable::default()` branch, and the gate never pays the parse in SPD-1.
      **Fix:** seed the gate's home with a representative `models.toml`.
      **Assessment:** trivial, and it is the general principle worth adopting —
      gates should model a *used* installation, not a fresh one. Worth a sweep of
      the other fixtures for the same assumption.

---

## SPD — Speed

- [x] **SPD-2 · P0 · [code] · 2–3 days** — tool output has no rate limit at any
      layer.
      `crates/rw-tools/src/bash.rs:1965` (`copy_stream`) reads child output in
      8 KiB slices and emits one durable `ToolOutputDelta` per read with no
      time-based batching. On the client, `packages/tui/src/app.ts:5472`
      (`isPresentationStreamDelta`) lists only `text_delta`, `thinking_delta`,
      `citation_delta`, `compaction_text_delta`, `compaction_thinking_delta`.
      **`tool_output_delta` and `subagent_progress` are not in that list**, so each
      chunk takes the `deferToFrame = false` branch at `app.ts:1158` and flushes
      synchronously through `#bindStateToComponents` — re-binding transcript,
      output viewer, subagent tray, context panel, interaction panel, review panel
      and composer, every time. A `cargo build` emitting 20 MB is roughly 2,500
      full synchronous reconciles with no frame gate.
      **Fix:** invert the predicate — coalesce by default, present immediately only
      for events with synchronous UI effects (approval prompts, terminal handover,
      focus changes). Then add a byte/time accumulator engine-side so one screen
      refresh of output is one event.
      **Assessment:** this is the item users actually feel, and the frustrating part
      is that the 16 ms coalescing machinery in `presentation.ts` is already built
      and correct — the highest-frequency event in the system simply was never
      routed through it. Do the predicate inversion first (hours, large win), then
      the engine-side accumulator as a separate brief; they are independently
      valuable and independently reviewable.

- [x] **SPD-1 · P1 · [verified] · 1–2 days** — a 1.6 MB pricing table is parsed
      synchronously on the startup path.
      `crates/rw-runtime/src/session_host.rs:325` reads and parses
      `~/.rottweiler/models.toml` before host readiness. On this machine that is
      **1.59 MB / 58,287 lines / ~4,000 model tables**, of which a session consults
      a handful. Measured: `rw --version` 7.0 ms, `rw config check` 7.9 ms,
      `rw models list` **42.9 ms** — the delta is the parse.
      **Fix:** move the catalog into the SQLite store already shipped (rusqlite is a
      dependency) and query by model id; or keep TOML and parse lazily on first
      pricing lookup, off the readiness path.
      **Assessment:** highest ratio of win to risk in the speed section. Likely
      explains most of the `engine_ready_p99` gap between platforms once a user has
      a real home directory — though per MSR-3 neither platform pays it in CI, so
      land MSR-3 first or you will not see the improvement.

- [x] **SPD-5 · P1 · [verified] · 1–2 days** — the tree-sitter runtime is rebuilt
      from scratch on every launch.
      `packages/tui/src/tree-sitter-runtime.ts:207` (`materializeTreeSitterRuntime`)
      runs each start: `readdirSync` over the entire system temp directory to sweep
      stale runtimes, a fresh `mkdtemp`, then zstd-decompress and write ~50 asset
      files — all 24 grammars, whether or not the session shows a single code
      block — deleted on exit. Measured **~65 ms**, repeated on every RSS recycle
      (UI-1).
      **Fix:** content-address into `~/.rottweiler/cache/tree-sitter/<blake3>/` and
      reuse (first launch pays, later ones `stat`). Then register grammars lazily,
      on first use of a language.
      **Assessment:** two fixes with different payoffs — caching buys startup time,
      lazy registration buys idle memory and helps UI-1. Do both, but they are
      separable if you want the smaller brief first. Keep the existing hardening
      (uid/mode checks, no symlink follow) intact in the cached path; that code is
      careful and should be ported, not rewritten.

- [x] **SPD-3 · P1 · [code] · 1–2 days** — the reducer rebuilds and sorts the whole
      tool record on every output chunk.
      `packages/tui/src/state/reducer.ts:33` (`retainRecentTools`) runs per chunk
      and does a full `{...current}` spread, `Object.entries` twice, two filters, a
      `Map` build over todo checkpoints, and a sort — all to enforce a 16-entry cap
      that can only change when a tool starts or finishes.
      `packages/tui/src/components/transcript.ts:1686` compounds it:
      `presentableTranscript(state)` (`transcript.ts:2080`) filters the entire
      retained transcript and builds two `Set`s over all tools and subagents, then
      slices to 16.
      **Fix:** separate append from eviction — appending a chunk mutates one
      projection's chunk list; eviction runs only on `tool_call_started` /
      `tool_call_finished`. Memoize `presentableTranscript` on the identities it
      actually depends on.
      **Assessment:** scoped deliberately. The card-level churn this used to cause
      was already fixed by the incremental `TurnCardViewModel` work in `b1b4a48`;
      what remains is the per-chunk allocation in the reducer and the projection
      recompute above it. Real, but its impact shrinks a lot once SPD-2 lands, so
      sequence it after and re-measure before committing effort.

- [x] **SPD-4 · P2 · [verified] · ~2 hours** — the SSE parser walks the stream one
      byte at a time and re-encodes every line to measure it.
      `packages/tui/src/transport/sse.ts:41` iterates `for (const byte of chunk)`
      into a plain number array, then `Uint8Array.from()` copies it for decoding.
      At `sse.ts:88` the decoded string is passed through
      `this.#encoder.encode(value)` solely to read `.byteLength` for a limit check —
      a full second UTF-8 pass, discarded. Benchmarked per 8 KiB frame:
      **0.098 ms** current vs **0.002 ms** with `indexOf(0x0a)` + `subarray`
      (≈250 ms vs ≈4 ms per 20 MB of tool output).
      **Fix:** scan with `indexOf`, slice with `subarray`, decode once; take byte
      length from slice bounds.
      **Assessment:** a 50× win on a ~15-line change, but be honest about scale — it
      is a quarter-second per 20 MB, secondary to SPD-2. Worth doing because it is
      nearly free, not because it is urgent. Good first Codex brief.

- [x] **SPD-6 · P1 if confirmed · [open] · 1 day to settle** — the release profile
      optimizes for size on a project whose thesis is speed.
      `Cargo.toml` sets `opt-level = "z"`; `scripts/cargo-release.sh` selects `z` on
      macOS, `s` on Linux, with a comment claiming Apple startup "is fastest with
      the smaller z profile". The introducing commit `a309952` says otherwise:
      *"Optimize that broad surface for distribution size; release performance gates
      protect the startup and turn-latency budgets."* That guarantee is circular —
      the baselines were measured *from* the size-optimized build, so the gate can
      only catch regressions relative to whatever `z` already costs.
      **Experiment:** build twice with `lto = "thin"` / `codegen-units = 16` to keep
      build time sane, once at `opt-level = "z"` and once at `3`; compare
      `engine_ready` and `headless_print`.
      **Assessment:** I did not resolve this — a cold LTO-fat build was too
      expensive to run here, and I am not willing to assert a number I did not
      measure. What I will assert is that the *rationale* in `cargo-release.sh` is
      post-hoc: the commit message shows the decision was made for distribution
      size. Run the experiment; if `3` wins, fix the comment as well as the setting.

- [x] **SPD-7 · P2 · [verified] · ~1 hour** — the Homebrew cask adds a bash process
      to every invocation.
      Because the cask sets `ROTTWEILER_PACKAGE_MANAGER` in the environment,
      Homebrew generates a shell wrapper instead of a symlink. Measured:
      direct binary `rw --version` **6.4 ms**, through the wrapper **8.5 ms**
      (process-spawn floor on this host is 2.5 ms).
      **Fix:** detect the package manager from the binary's own install path at
      runtime and drop the env var, so the cask installs a plain symlink.
      **Assessment:** 33% overhead sounds worse than it is — nobody perceives 2 ms.
      Do it because it is an hour and because "we ship a shell wrapper in front of
      our fast binary" is embarrassing for a project with this thesis, not because
      it moves the needle.

---

## UI — Interface stability

The TUI itself renders well: clean composition, 34 themes, a live session came up
correctly on the first attempt. Both items below are underneath it.

- [x] **UI-1 · P0 · [verified] · 1 day to diagnose, then scope** — a memory leak is
      handled by killing and respawning the UI mid-session.
      `packages/tui/src/index.ts:213` polls RSS every 100 ms; above **384 MB** it
      sets `process.exitCode = 75` and destroys the renderer.
      `crates/rw-cli/src/supervisor.rs:576` treats exit 75 as "a deliberate
      process-local memory recycle" and respawns with no backoff. The user gets a
      full TUI restart — reconnect, transcript re-projection, another ~390 ms to
      first frame, another ~65 ms of grammar materialization.
      The code comment names the suspected cause honestly: *"OpenTUI's native
      allocator can retain released render graphs during very long tool-heavy
      sessions."* That is a hypothesis, not an isolation — and the transcript code
      does call `destroyRecursively()` and pool cards, so it may be an upstream
      OpenTUI bug rather than misuse here. Headroom is thin either way: an idle TUI
      with no session work measured **185 MB RSS**, 48% of the threshold before
      anything happens, because 24 tree-sitter WASM grammars compile eagerly at
      startup (nine `Wasm Worklist Helper` threads visible in a sample).
      **Fix, in order:** (1) reproduce in isolation — a minimal OpenTUI harness that
      mounts and destroys renderables in a loop while watching RSS; that settles
      upstream-bug vs. misuse in an afternoon. (2) Land SPD-5's lazy grammar
      registration, which buys headroom regardless of the answer. (3) Keep the
      recycle only as a last-resort net, and make it preserve scroll position and
      composer contents.
      **Assessment:** I want to be fair to the existing code — a supervised recycle
      is a *defensible* safety net, and the durable-state design is what makes it
      survivable at all. The problem is that it is currently the entire answer to an
      undiagnosed leak, and it is load-bearing enough that the soak baseline
      (600 MiB combined ceiling) is built around it. Step (1) is one focused day and
      changes what the other steps should be, so do not commit to a remediation
      shape before running it.

- [x] **UI-2 · P1 · [open] · ~2 hours to confirm** — the context meter showed a zero
      denominator on a live subscription session.
      Capturing the real TUI against your `openai_codex` config, the status line
      read `◉ execute │ model fast │ ctx 3.9k/0 (—%) │ $0.000 │ cache —`. That is the
      shape `docs/gaps/02-context-compaction.md` GAP-02-01 describes, which is marked
      **Resolved (2026-07-12)**. The consequence matters more than the cosmetics:
      `rw-context/src/budget.rs` treats a zero window as "no limit", so automatic
      compaction cannot trigger.
      **Confirm:** run one full turn on the subscription route and check whether
      `context_usage_updated` carries a non-zero `usable_tokens`.
      **Assessment:** flagged as open, not as a regression — I observed one session
      start, not a full turn, and the meter may simply be pre-population. But it is
      the failure mode most likely to bite you personally on your own config, and
      confirming it costs two hours. Do this before the bigger items.

---

## COR — Correctness

- [x] **COR-1 · P1 · [code] · ~2 hours** — multi-byte characters straddling an
      8 KiB read boundary are corrupted, in the model's input and not just the
      display.
      `crates/rw-tools/src/bash.rs:1982` does
      `String::from_utf8_lossy(&buffer[..read]).into_owned()` and `copy_stream`
      keeps no carry buffer between reads, so a boundary falling mid-sequence turns
      those bytes into `U+FFFD`. This is not display-only: `bash.rs:2445` builds the
      captured output from the already-lossy chunk strings, so the replacements are
      baked into the tool result the model receives. Any build log with box-drawing
      characters, non-ASCII paths, or emoji test output corrupts roughly once per
      8 KiB.
      **Fix:** hold a `Vec<u8>` carry buffer; use
      `std::str::from_utf8(&buf).unwrap_err().valid_up_to()` to emit only the
      complete prefix and prepend the remainder to the next read.
      **Assessment:** tagged `[code]` deliberately — I traced the path but did not
      construct a failing input, so land it with a regression test that feeds a
      multi-byte character across a forced boundary. Reachability is close to
      certain and the blast radius is larger than it first appears, because it
      silently degrades what the model reads rather than producing a visible error.

- [x] **COR-2 · P1 · [verified] · half a day** — hard-killing the supervisor orphans
      its children and locks the workspace.
      `SIGKILL` to the `rw` supervisor only; 40 seconds later both children were
      alive and reparented to init (`ppid 1`): the engine at 50 MB RSS and
      `rottweiler-tui` at 185 MB. The orphaned engine keeps holding
      `execution.lock`, so the next launch in that workspace fails with
      `Resource temporarily unavailable (os error 35)` and a message naming a lock
      file path but offering no recovery. `SIGTERM` cleans up correctly.
      **Fix:** put children in the supervisor's process group and have each watch
      for parent death (`PR_SET_PDEATHSIG` on Linux, kqueue `NOTE_EXIT` on macOS).
      Separately, make the lease self-healing — if the recorded owner PID is gone,
      reclaim it and say so.
      **Assessment:** the exact case this hits is crash, OOM, and force-quit, i.e.
      precisely when the user is already having a bad time and least able to debug a
      lock file path. The two halves are independently valuable; if only one gets
      done, do the self-healing lease, because it also covers machine-crash and
      stale-NFS cases that process-group cleanup cannot.

---

## PLG — Extensibility

These are not five problems. They are one architectural decision with four
downstream consequences.

- [ ] **PLG-1 · P0 · [verified] · hours** — the official scaffold cannot be
      installed, and CI patches around the failure.
      `rw plugin scaffold` generates a `package.json` depending on
      `@rottweiler/plugin@^0.1.0`. That package is not published:
      `bun install` returns `GET https://registry.npmjs.org/@rottweiler%2fplugin - 404`.
      The conformance gate cannot catch it, because `.github/workflows/ci.yml:83`
      rewrites the dependency to `file:$GITHUB_WORKSPACE/packages/plugin-sdk`
      before installing. Even with the local path substituted, the build still
      fails unless the SDK's `dist/` was built first — which the scaffold's
      instructions never mention.
      **Fix:** publish `@rottweiler/plugin` to npm from the release workflow; change
      the CI conformance step to install the *published* package.
      **Assessment:** do this first, ahead of everything else in this document. The
      extensibility pillar has no working front door today and no gate can tell you
      that. It is also the cheapest P0 here. The deeper lesson is the general one:
      a gate that rewrites the artifact under test is not testing the artifact —
      worth grepping CI for other instances.

- [ ] **PLG-2 · P0 · [verified] · design pass first, then large** — a hello-world
      plugin compiles to a 64 MB executable, and every edit invalidates its approval.
      Building the generated scaffold with the SDK linked locally produced
      `dist/plugin` at **63,943,538 bytes** — an entire embedded Bun runtime, per
      plugin. The cause chain is a single decision: approval binds
      `ExecutableIdentity` — canonical path, length and BLAKE3 of the binary
      (`crates/rw-ext/src/plugin_runtime.rs:230`). Attesting an opaque compiled
      artifact *requires* the compile step, which requires embedding Bun, which
      produces 64 MB, and which means every source edit changes the hash and
      invalidates approval. Five plugins is 320 MB on disk and five Bun processes at
      runtime. Compare Pi, where a plugin is a `.ts` file you drop in a directory.
      **Fix direction — attest source, not a binary.** You already ship a sandboxed
      Bun runtime: the TUI. Add a harness-owned TS plugin host that loads plugin
      modules into isolated workers, and pin approval to the BLAKE3 of the source
      file plus its lockfile.
      **Assessment:** the largest item here and the one that decides whether
      "pi-grade extensibility" is true. Two things worth stating plainly. First,
      this does **not** reverse ADR-019's rejection of an embedded scripting
      language — the proposal is a separate host *process*, so crash isolation
      survives and RPC plugins in other languages keep working unchanged; TS gets a
      first-class path, not a monopoly. Second, the security properties survive
      intact: same capability manifest, same approval ledger, same re-prompt on
      capability change — you are changing *what is hashed*, not whether it is
      hashed. That said, it is a real design pass with real supply-chain surface
      (transitive npm deps are no longer frozen into one attested artifact), so it
      needs a written design before any code. Do not let a Codex brief start here.

- [ ] **PLG-4 · P1 · unblocked by PLG-2 · medium** — `rw plugin dev` cannot attach
      to a live session.
      Reading `crates/rw-cli/src/plugin_dev.rs`, it launches the plugin, runs the
      handshake, traces RPC and restarts on file change (200 ms poll, 400 ms
      debounce) — but it never attaches to a running session. So the iteration loop
      for "does my hook do the right thing against real transcript state" is still
      build → register → approve → restart.
      **Fix:** let `rw plugin dev` attach to the running engine as a
      development-scoped plugin with hot reload, still under the restrictive
      sandbox and still without touching production approval.
      **Assessment:** this is the feature that would actually feel like Pi, and it
      is mostly gated on PLG-2 — with source attestation, hot reload becomes
      natural instead of a special case. Worth writing into the PLG-2 design as a
      requirement rather than treating it as a follow-up, because designing the host
      without it will produce a host that cannot do it.

- [x] **PLG-3 · P2 · [verified] · ~half a day** — the capability manifest is
      declared twice and hand-synchronized.
      The scaffold emits the same tool and hook declarations in both `manifest.json`
      and the `definePlugin({ manifest: … })` call in `src/index.ts`. The host
      compares them and rejects on divergence
      (`crates/rw-ext/src/plugin_runtime.rs:1573`, "initialized manifest differs
      from approved manifest").
      **Fix:** generate `manifest.json` from the `definePlugin` call as a build
      step, so the declaration exists once, in the code the author is editing.
      **Assessment:** small, but it is a first-five-minutes papercut — a new author's
      likely first encounter with the protocol is a mismatch error they caused by
      editing one of two copies. Bundle it with PLG-1 since both touch the scaffold.

---

## Checked and cleared

Recorded so these do not get re-litigated in a later round.

- **Transcript retention and mounting are correct.** `MAX_MOUNTED_TRANSCRIPT_ENTRIES = 16`,
  `retainTranscriptEntry`, and `MAX_RETAINED_TOOL_PROJECTIONS = 16` genuinely bound
  state; `#reconcileHistory` pools and destroys cards rather than leaking them. The
  windowing is not the source of UI-1.
- **The presentation controller is well built.** The 16 ms coalescing, idle-frame
  fast path, and replay suspend/coalesce in `packages/tui/src/presentation.ts` are
  right. SPD-2 is an under-use of this code, not a defect in it.
- **`fsync`-per-durable-event is not the bottleneck I first suspected.** Measured at
  **0.066 ms** median on this host — about 0.17 s per 20 MB of tool output. Real but
  secondary; it would matter much more on a slow or networked filesystem, so leave
  the durability guarantee alone.
- **`uds_event_p99_us` is a fair metric.** It measures a full authenticated
  `POST /v1/command` → SSE round trip including Python-side overhead, not a raw
  socket write. No change needed.
- **Incremental transcript card rendering is done.** Shipped in `b1b4a48` /
  `3d31639`; resize no longer rebuilds history. SPD-3 is scoped to what remains
  above it.

---

## Suggested order

| # | Work | Items | Size |
|---|------|-------|------|
| 1 | Publish the SDK; make CI install the published package | PLG-1, PLG-3 | hours |
| 2 | Confirm the subscription context meter | UI-2 | ~2 h |
| 3 | Fix the three broken metrics before optimizing against them | MSR-1, MSR-2, MSR-3 | 1–2 d |
| 4 | Route tool output through the frame gate that already exists | SPD-2 | 2–3 d |
| 5 | Stop parsing and writing what the session never reads | SPD-1, SPD-5 | 2–3 d |
| 6 | Isolate the OpenTUI leak instead of living with the recycle | UI-1 | 1 d to diagnose |
| 7 | Land the two correctness fixes | COR-1, COR-2 | ~1 d |
| 8 | Re-measure, then decide whether SPD-3 is still worth it | SPD-3 | 1–2 d |
| 9 | Design the in-process TS plugin host | PLG-2, PLG-4 | design first |
| 10 | Settle the release profile; cheap wins | SPD-6, SPD-4, SPD-7 | 1 d |

Rationale for the ordering: (1) is the only fully broken pillar and the cheapest
P0. (2) is short and concerns your own daily config. (3) comes before (4)–(5)
because otherwise those land with no way to prove they worked. (8) sits after a
re-measure on purpose — SPD-2 may absorb most of it. (9) is the only item that
should not start as a Codex brief.

---

## Method

- Startup timings: shipped binaries driven under a pty with the
  `ROTTWEILER_FIRST_PAINT_MARKER` hook, output bursts timestamped, 6–15 trials
  each, medians reported.
- Thread and stack attribution: `sample(1)` against the live TUI **with an active
  pty reader** — without one, `tcsetattr` drain time dominates the profile and is
  easily mistaken for work.
- SSE parser comparison: both implementations benchmarked in Bun 1.4.0, 500
  iterations on an 8 KiB frame, warm.
- Plugin flow: `rw plugin scaffold` run for real, then `bun install` and
  `bun run build` against both the npm registry and a locally linked SDK.
- Orphan behaviour: `SIGKILL` to the supervisor only, children observed via `ps`
  after 40 s.

The original open experiments are now resolved: SPD-6 has a controlled 100-sample
comparison plus a qualified fat-LTO build, and UI-2 has a full-turn subscription
capability regression test with a non-zero usable context window. PLG-1's external
registry publication and the PLG-2/PLG-4 production phases remain open for the
reasons recorded in the resolution section.
