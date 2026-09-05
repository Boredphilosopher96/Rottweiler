# Source preparation ownership and native resolver evidence

This is the preparation foundation for ADR-035. It does **not** complete dormant
plugin activation or a factory-wide startup governor.

`SourcePreparationBudget` charges 32 admitted jobs and two executing helpers.
`SourcePreparations` owns the jobs of one resolver/generation; unrelated owners
can share capacity without sharing cancellation barriers. Production currently
creates that budget per resolver. Factory-wide sharing belongs to activation.

The owned driver retains scratch, native process authority and both output
readers through actual process-tree settlement. Cancellation, future drop,
30-second immutable deadline and output overflow revoke execution. Unproven
settlement or executor panic retains capacity and leaves the barrier incomplete.
Completed jobs retire without another call; undelivered results retain admission
until consumed or dropped. Each output stream accepts at most 1 MiB of data.

A failed production stdio attachment now settles its spawned process tree before
returning an error. The regression launches a real descendant and checks its
post-error process state without issuing a test-side cleanup signal.

## Native macOS result

With Bun 1.3.14 (0d9b296a), the new production test compiles the current source
host, runs graph discovery and bundling through `SandboxedPluginLauncher`, checks
the sealed bundle digest/attestations, and proves both helpers have exited.
The sealed output survives scratch deletion.

The first real run exposed an existing resolver failure:

```
Bundle failed
Cannot read directory "/private/var/folders/": PermissionDenied
```

Bun enumerates canonical ancestor directories even for package-local imports.
An explicit tsconfig override did not remove this lookup. Preparation alone now
uses macOS literal-directory grants for at most 64 ancestors of its declared
code directory. It does not grant descendants or file contents. A native shell
regression proves ancestor listing succeeds while sibling file reads and nested
directory enumeration fail. Ordinary plugin execution does not request the grant.
Bounded aggregate build diagnostics now retain the useful cause instead of only
`Bundle failed`.

## Original Linux limitation, reproduced

The existing Docker daemon is Linux arm64. Its x64 emulation returned ENOSYS for
Landlock; that run is not evidence of Linux sandbox behavior. A native arm64
Bun 1.3.14 container supported Landlock ABI 4. The checked-in
`linux-source-landlock-probe.ts` installs a strict filesystem policy and then
execs the freshly compiled host, so all compiler threads inherit the restriction.
Its read-root list matches the production system roots relevant to the fixture,
plus the declared package; only the separate output directory is writable.
This is a diagnostic policy reproduction, not the full production network,
credential exclusion, helper attestation or release qualification path.

For a package outside the system-readable `/tmp` subtree:

```
{"bun":"1.3.14","arch":"arm64","landlockABI":"4","root":"/home/plugin-user/packages/example","ancestorGrant":false}
Bundle failed
Cannot read directory "/home/plugin-user/": AccessDenied
```

No Linux permissions were broadened. Landlock `ReadDir` beneath an ancestor is
recursive; it cannot implement the macOS exact-directory rule. Linux preparation
required a controlled filesystem view or hermetic resolution at checkpoint
`6613b17`. The controlled-view unit below addresses that source-path failure. No native x64 performance or release claim follows from this experiment.

To reproduce on the existing Docker host, use `oven/bun:1.3.14` with
`--platform linux/arm64 --security-opt seccomp=unconfined` (the ordinary Docker
seccomp profile blocks the Landlock query). Copy the current `packages/plugin-host`
and `packages/plugin-sdk` into `/workspace/packages/` in a disposable container,
replace the host package's local SDK link with `/workspace/packages/plugin-sdk`,
and copy the probe to `/probe.ts`. Build and invoke:

```sh
mkdir -p /home/plugin-user/packages/example /tmp/source-output
printf '%s\n' '{"dependencies":{"@rottweiler/plugin":"0.1.0"}}' > /home/plugin-user/packages/example/package.json
printf '%s\n' '{"name":"source-probe"}' > /home/plugin-user/packages/example/manifest.json
printf '%s\n' "import manifest from './manifest.json'; export default () => manifest;" > /home/plugin-user/packages/example/index.ts
bun build --compile /workspace/packages/plugin-host/src/index.ts --outfile /usr/local/bin/source-host
cd /home/plugin-user/packages/example
bun /probe.ts /usr/local/bin/source-host graph "$PWD" "$PWD/index.ts"
```

## Checks

Pinned Bun commands run with its directory first in `PATH`:

```sh
cargo test -p rw-runtime source_plugin -- --nocapture
cargo test -p rw-runtime -p rw-sandbox
cargo clippy -p rw-runtime -p rw-sandbox --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
python3 scripts/check-ownership.py
(cd packages/plugin-host && bun run typecheck && bun test)
```

The focused source suite passed 12 tests, including seven ownership regressions,
the two native sandbox/resolver regressions, and existing source validation tests.
The host package passed four tests and typechecking. The full runtime suite
passed 228 tests (one manual long-session benchmark ignored); the sandbox suite passed 20.
The two runtime subprocess fixture reruns are duplicate checks, not extra tests.

## Linux controlled preparation view

`PreparationFilesystem` now gives the compiler a private filesystem namespace.
The code root is read-only at `/plugin`; work and output are separate writable
roots at `/scratch` and `/output`. The helper pins directory device/inode
identities, rejects overlapping roots, masks credential paths and home directories,
and bounds synthesized directories and mounts to 512 nodes and 8,192 inspected
entries. It copies no package contents. A separate private proc mount exposes only
its PID namespace. File descriptors into the outer filesystem close before exec.
The helper removes capabilities, denies mount-changing syscalls, and retains the
Landlock write and network floors. Preparation uses no egress proxy.

The source preparation driver owns each private work/view directory through
process-tree settlement. Panic or an aborted owner cannot release the directory
or return admission. Ordinary approved plugin execution keeps its existing policy.

The native Linux arm64 Rust helper driver passed source-read, source-write-denial,
credential-alias-denial, hidden-host-root, zero-capability, and denied-mount checks.
The same helper ran the current Bun 1.3.14 compiled source host for graph discovery
and bundling and produced matching input reports. The source host's entry file
SHA-256 was `2951f44263afe439edae30c1f44022220ef8a9a546bdfa5287fd5fd5c758def8`.

The local Docker daemon required a disposable privileged container to exercise
nested user/mount/PID namespaces. The earlier non-root namespace probe failed
with `write failed /proc/self/uid_map: Operation not permitted`. These runs prove
the view implementation on native arm64 Linux when its namespace prerequisites
are available. They do not prove ordinary-user availability on that Docker host,
native x64 performance, a protected soak, or release qualification. Production
refuses execution when required namespace or sandbox setup fails.

The checked-in acceptance drivers are:

```sh
ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 \
ROTTWEILER_PREPARATION_TEST_HOST=/path/to/current/rottweiler-plugin-host \
cargo test -p rw-sandbox --test linux_preparation_driver
ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 \
ROTTWEILER_PREPARATION_TEST_HOST=/path/to/current/rottweiler-plugin-host \
cargo test -p rw-runtime --test linux_source_preparation
```

The runtime driver uses `resolve_plugin_process` and the production launcher,
then verifies sealed bundle bytes and the complete source/executable attestation.
Without an explicit compiled host, it builds the current plugin host with Bun.
The local fixture's Rust toolchain is 1.97.1 and its Bun version is 1.3.14.

Current-view verification passed the 12 focused macOS source tests, all 20 macOS
sandbox tests, and strict runtime/sandbox Clippy across all targets and features
on both macOS and native arm64 Linux. Linux passed all 19 sandbox unit tests,
16 existing egress integration tests, both helper acceptance drivers, and the
production runtime source-preparation driver. The disposable Rust container
needed `iproute2` and Python installed before the existing egress canaries could
run; the first missing-prerequisite failures were retained in the run evidence.

The additional Linux runtime unit-test executable failed to link because the
container killed GNU `ld` with signal 9. Its seven preparation ownership tests
are verified on macOS; no Linux unit-runtime execution is claimed. The smaller
production Linux runtime integration executable did link and pass. Formatting,
source ownership, and whitespace checks also passed.
