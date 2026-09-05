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

## Linux limitation, reproduced

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
needs a controlled filesystem view or hermetic resolution in the next A28/A25
unit. Source plugins at these paths remain unavailable on Linux until that unit
lands. No native x64 performance or release claim follows from this experiment.

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
