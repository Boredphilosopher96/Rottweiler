# TypeScript source plugin host

**Status:** accepted design, 2026-08-22. The SDK publication and manifest-front-door
changes may land independently. The production host starts with the feasibility
spike in the migration plan; this document does not claim that host is implemented.

## Outcome

Rottweiler will ship one private, release-owned TypeScript plugin host and start
one sandboxed host process for each active TypeScript plugin. A plugin remains a
small source package; it no longer embeds a separate Bun runtime. Rust continues
to own trust, approval, preparation, sandbox policy, registration, credentials,
replay, and process lifecycle.

Production executes a sealed, content-addressed ESM bundle prepared from an exact
source and dependency graph. Development may execute source under a temporary,
session-scoped grant. The generic executable JSON-RPC plugin tier remains
supported for every language, including TypeScript authors who deliberately want
a standalone executable.

This preserves ADR-003: the Rust engine does not embed JavaScript, unrelated
plugins do not share a process or authority, and the existing newline-delimited
JSON-RPC contract remains the engine-facing boundary.

## Author contract

A conventional source package contains:

```text
plugin/
  manifest.json
  package.json
  bun.lock
  src/index.ts
```

`manifest.json` is the single authored capability declaration. The module imports
that inert data through `parsePluginManifest` and passes the result to
`definePlugin`. Rust can therefore display and approve authority without executing
unapproved TypeScript. Runtime initialization must return the same manifest
fingerprint.

Generating the manifest by importing `definePlugin` is rejected. Importing a
TypeScript module runs its top-level code before the capability review. A static
extractor would either implement a surprising TypeScript subset or grow into an
evaluator. One inert JSON document gives authors one declaration without crossing
the approval boundary.

The production loop becomes:

```sh
bun install --frozen-lockfile --ignore-scripts
bun test
rw plugin check .
rw plugin approve <name>
rw
```

No author-run `bun build --compile` is required for the source-host path. Package
install scripts remain an author-controlled supply-chain risk; preparation does
not make them safe.

## Ownership and module seam

The first production change is deliberately narrow. A new resolver converts one
configured artifact into the values the current runtime already consumes:

```rust
enum PluginTarget {
    Executable(PluginProcessConfig),
    TypeScript(TypeScriptSourcePackage),
}

struct ResolvedPlugin {
    manifest: PluginManifest,
    process: PluginProcessConfig,
}

impl PluginResolver {
    fn resolve_production(&self, target: &PluginTarget) -> Result<ResolvedPlugin, ResolveError>;
}
```

After resolution, the existing approval store, `PluginLauncher`, `PluginHost`,
capability enforcer, RPC adapters, host-mediated provider HTTP, redaction, and
shutdown path remain authoritative. Session startup does not coordinate source
copying, bundling, cache publication, or helper authentication itself.

New responsibilities live in one deep runtime module:

- authenticate and supervise the private host;
- discover and freeze the complete source graph;
- validate lockfile coordinates and input policy;
- publish and validate sealed bundles;
- resolve the bundle to the existing process configuration.

The host is a private sibling in the application bundle. It is never resolved
from `PATH` and is not a public command. No configured TypeScript source plugin
means no host process or preparation work on normal startup.

## Preparation protocol

Preparation is a two-pass operation so an editor cannot race the provenance
claim:

1. Authenticate the release-owned host before running it.
2. Run graph discovery with read-only package access, scratch-only writes, no
   network, a cleared environment, fixed Bun options, bounded output, a deadline,
   and process-tree supervision.
3. Validate every reported logical path, package coordinate, import kind, size,
   depth, case-fold uniqueness, and supported dependency form in Rust.
4. Match every third-party resolution to the committed Bun lockfile's package,
   version, and integrity record.
5. Open accepted inputs without following links. Hash and copy the same bytes into
   a new private staging tree.
6. Build again from staging with fixed target, format, conditions, and defines.
   Build plugins, unresolved externals, remote imports, native addons, computed
   imports, ambient package caches, and runtime package installation are denied.
7. Require the second normalized input graph to equal the discovery graph.
8. Hash the graph and output bundle in Rust, fsync files and parents, then publish
   the immutable cache entry with one atomic rename.

The source graph uses normalized logical paths and locked package coordinates;
absolute checkout and package-cache paths do not affect its digest. The detailed
input report remains beside the bundle. The approval record contains bounded
summaries and digests rather than thousands of path records.

Preparation limits cover input count, total bytes, depth, report bytes, output
bytes, and wall time. The first values must come from measured fixtures. Unsupported
dependency or import forms fail before approval instead of silently escaping the
bundle.

## Identity and sandbox rules

Production approval binds:

- plugin origin and canonical inert manifest fingerprint;
- normalized source graph and lockfile digests;
- sealed bundle digest;
- source-host semantic ABI and bundle format;
- configured environment names, domains, working directory, and sandbox policy.

The installed `rw` authenticates the exact sibling host digest before preparation
and launch. Release provenance and the session launch receipt record the exact
host, bundle, ABI, and product version. A change to parsing, bundling, bootstrap,
or execution semantics increments the ABI and requires reapproval. A compatible
official host rebuild remains authenticated as part of the Rottweiler generation.

The private bundle cache is executable input, not workspace authority. A
TypeScript-only `PreparedCodeRootIdentity` proves that the exact bundle directory
is below the owner-private bundle store. The launcher adds only that directory to
intrinsic code reads. It never adds the store to approved workspace roots or write
roots, so `reads-fs` and `writes-fs` cannot grant access to other cached plugins.

New TypeScript-only identity fields are omitted for direct executable plugins.
Golden tests must prove their existing serialized approval fingerprints do not
change.

## Runtime and failure containment

Each active plugin gets a separate host process, process group, sandbox, scratch
directory, RPC state, and egress policy. The operating system may share immutable
host code pages, but heaps and authority remain separate.

A source-plugin preparation, approval, launch, or handshake failure marks only
that plugin unavailable. It does not stop the engine or tear down unrelated
plugins. A production crash retains an unavailable generation with the approved
declarations:

- fail-closed hooks remain registered as tombstones that reject with
  `plugin_unavailable`;
- tools, commands, and providers remain discoverable but return a bounded
  unavailable error;
- event delivery stops;
- pushes from retired generations fail.

Every failure path kills and reaps the complete process tree. A malformed cache
entry is ignored or quarantined and never executed. Cache recreation must either
produce the approved identities or require a new approval.

## Live-session development

Live attachment is a second implementation phase, not a prerequisite for the
production resolver. Its shape is fixed now so the host does not block it later:

```text
rw plugin dev . --session current --allow-dev-exec
```

The CLI authenticates to a local engine and requests a temporary attachment. The
engine owns preparation and watching; the CLI never supplies trusted hashes or
bundle paths. The in-memory grant binds the session, plugin name, project-root
identity, manifest and config fingerprints, connection, driver lease, and an
explicit capability ceiling. The first version denies remote attachment,
workspace writes, subprocesses, and network unless separately granted.

Only the session actor activates or replaces a generation. Preparation, handshake,
provider discovery, and collision checks occur off-actor. At a between-turn safe
point the actor swaps one immutable `SessionExtensionSnapshot` containing tools,
hooks, commands, providers, event routes, pushes, plugin generations, and a
revision. A turn captures one snapshot and cannot straddle registry generations.

A source-only edit within the grant creates a candidate generation. Syntax,
preparation, handshake, or collision failure retains the last good development
generation. Authority expansion requires a new attachment grant. In-flight calls
pin their old generation. Disconnect, heartbeat expiry, lease loss, session close,
or engine shutdown detaches development, launches the approved production bundle,
and reaps the development child. Durable activation events contain digests and
generation numbers, never local paths.

The current standalone `plugin dev` supervisor may remain as a diagnostic until
this actor-owned attachment passes end-to-end acceptance. It is not the final live
development experience.

## Distribution and SDK publication

The official SDK is published from the exact tag workflow using npm trusted
publishing. The package version must equal the release version. CI packs the
candidate package and consumes its tarball instead of rewriting the scaffold to a
workspace source path. After qualification, the release publishes and then creates
a clean scaffold that installs the unmodified version from the public registry,
typechecks, tests, and builds.

The source host phase adds the private helper to release archives, installers,
updater allowlists, Homebrew private trees, WSL acceptance, size gates, SBOM, and
provenance. Extracted-archive acceptance runs preparation, approval, one real tool
call, and shutdown with system `bun` and `node` absent from `PATH`. No-plugin
startup must prove that the helper is neither spawned nor mapped.

## Migration checkpoints

Each checkpoint ends in a directly verifiable state:

1. Publish and consume the SDK package; remove CI source substitution.
2. Make inert `manifest.json` the only authored declaration and validate its SDK
   import. Retain the current standalone executable path.
3. Prove a compiled private host can import one sealed external ESM module under
   the production sandbox on macOS and Linux. Stop if this fails.
4. Add logical source-graph, bundle, prepared-root, and approval identities. Prove
   direct-plugin fingerprints remain unchanged.
5. Add two-pass preparation and the private immutable cache without launching it
   in sessions.
6. Package and authenticate the inert helper across every distribution surface.
7. Add opt-in TypeScript `source` configuration and resolve it into the existing
   `PluginHost` path.
8. Change the scaffold quickstart to source mode. Keep the generic executable
   recipe available.
9. Add actor-owned live attachment and remove the standalone dev supervisor only
   after reload, last-good, detach, and process-reap acceptance passes.

## Required verification

- Clean public-registry scaffold install, typecheck, tests, and build.
- One manifest declaration; Rust and SDK validators accept and reject the same
  fixtures.
- Same source package in two checkout roots yields the same logical graph and
  bundle digests.
- Every reachable source, dependency, lock, manifest, ABI, or policy change
  changes identity; an unreachable file does not.
- Preparation never executes plugin top-level code.
- Symlink escapes, case collisions, native addons, dynamic imports, missing locks,
  unresolved externals, races, and bounds fail closed.
- Two plugins run in different PIDs and cannot read each other's source, cache,
  scratch, credentials, or network authority.
- One plugin crash retains fail-closed behavior and leaves other plugins and the
  engine alive.
- Direct executable protocol 1 and 2 plugins keep their approval fingerprints and
  behavior.
- Live attach invokes real tools and hooks, keeps the last good generation after a
  broken edit, blocks capability expansion, drains old work, restores production,
  and reaps on disconnect.
- Exact extracted release archives work without a system JavaScript runtime and
  include the authenticated helper in provenance and SBOM output.

## Non-goals

- No shared multi-plugin JavaScript daemon.
- No JavaScript runtime embedded in the Rust engine.
- No runtime dependency install, native addon, remote import, or arbitrary dynamic
  module resolution in the first source-host release.
- No production hot reload or silent approval carry-forward after source changes.
- No replacement of the generic any-language RPC tier.
- No claim that process isolation is a complete CPU or memory quota.
