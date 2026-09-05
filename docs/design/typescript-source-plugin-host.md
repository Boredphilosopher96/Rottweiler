# TypeScript source plugin host

## Execution model

Rottweiler ships one private, release-owned TypeScript plugin host and starts
one sandboxed host process for each active TypeScript plugin. A plugin remains a
small source package executed by the host's Bun runtime. Rust owns trust,
approval, preparation, sandbox policy, registration, credentials,
replay, and process lifecycle.

Production executes a sealed, content-addressed ESM bundle prepared from an exact
source and dependency graph. Development may execute source under a temporary,
session-scoped grant. The generic executable JSON-RPC plugin tier remains
supported for every language, including TypeScript authors who deliberately want
a standalone executable.

As specified in ADR-003, the Rust engine does not embed JavaScript. Unrelated
plugins have separate processes and authority. Newline-delimited JSON-RPC is the
engine-facing boundary.

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

The production workflow is:

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

The resolver converts a configured artifact into a process configuration:

```rust
enum PluginTarget {
    Executable(PluginProcessConfig),
    TypeScript(TypeScriptSourcePackage),
}

async fn resolve_plugin_process(
    plugin: &DiscoveredPlugin,
    private_root: &Path,
    helper: &Path,
) -> Result<PluginProcessConfig>;
```

After resolution, the approval store, `PluginLauncher`, `PluginHost`,
capability enforcer, RPC adapters, host-mediated provider HTTP, redaction, and
shutdown path remain authoritative. Session startup does not coordinate source
copying, bundling, cache publication, or helper authentication itself.

The source-host runtime owns:

- authenticate and supervise the private host;
- discover and freeze the complete source graph;
- validate lockfile coordinates and input policy;
- publish and validate sealed bundles;
- resolve the bundle to a process configuration.

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
bytes, and wall time. Limits are qualified against measured fixtures. Unsupported
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

Source-plugin approval binds the source graph and prepared bundle. Direct
executable approval binds the executable artifact and its declared authority.

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

Live attachment uses the same production resolver and process boundary:

```text
rw plugin dev . --session current --allow-dev-exec
```

The CLI authenticates to a local engine with a capability that can only attach or
detach a development plugin. It watches owner-controlled package inputs and asks
the actor to prepare a stable edit; it never supplies trusted hashes or bundle
paths. The runtime owns preparation, the in-memory grant, candidate generation,
capability ceiling, and process lifecycle. Development denies providers, pushes,
events, workspace writes, subprocesses, and network.

Only the session actor activates or replaces a generation. Preparation, handshake,
and collision checks complete before activation. At an idle, between-turn safe
point the actor swaps one immutable `SessionExtensionSnapshot` containing the
tool, hook, and command registries and a revision. A turn captures one snapshot
and cannot straddle registry generations.

A stable source edit within the grant creates a candidate generation. Syntax,
preparation, handshake, or collision failure retains the last good development
generation. Authority expansion requires a new attachment grant. In-flight calls
pin their old generation. Ctrl-C detaches explicitly; session or engine shutdown
kills the process-owned generation. A rejected candidate is shut down and reaped
before the last-good snapshot remains active.

## Distribution and SDK publication

The official SDK is published from the exact tag workflow using npm trusted
publishing. The package version must equal the release version. CI packs the
candidate package and consumes its tarball instead of rewriting the scaffold to a
workspace source path. After qualification, the release publishes and then creates
a clean scaffold that installs the unmodified version from the public registry,
typechecks, tests, and builds.

The source host is included in release archives, installers, Homebrew private
trees, size gates, and archive provenance. The release build executes the
extracted helper and verifies its semantic identity. Source acceptance separately
runs sandboxed preparation and approval with no system JavaScript runtime used at
execution time.


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
- Direct executable plugins satisfy the protocol and manifest contracts in
  ADR-031. Approval binds the complete manifest identity.
- Live attach invokes real tools and hooks, keeps the last good generation after a
  broken edit, blocks capability expansion, drains old work, restores production,
  and reaps on disconnect.
- Exact extracted release archives work without a system JavaScript runtime and
  include the authenticated helper in provenance and SBOM output.

## Non-goals

- No shared multi-plugin JavaScript daemon.
- No JavaScript runtime embedded in the Rust engine.
- No runtime dependency install, native addon, remote import, or arbitrary dynamic
  module resolution.
- No production hot reload or silent approval carry-forward after source changes.
- No replacement of the generic any-language RPC tier.
- No claim that process isolation is a complete CPU or memory quota.
