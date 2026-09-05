# macOS worker delegation policy

The process-creation policy now uses a deny-default Seatbelt profile on macOS. It permits executable loading, existing bounded file grants, read-only system queries, self process information/signals and the exact owned proxy route when configured. Ambient Mach lookup, external task access, AppleEvents and named IPC have no grant. Ordinary command policies retain their existing behavior. This mechanism is not yet enabled for configured plugins; host-mediated execution and inherited-authority clearing must land with that wiring.

Native ARM64 validation with pinned Bun 1.3.14:

- All 20 sandbox unit tests, the process/thread driver and the new delegation test pass. The delegation test first proves the same inherited bootstrap lookup and owned Unix socket succeed without the sandbox, then proves both are denied in deny-network and proxy modes. The exact proxy port remains connectable.
- Kernel policy checks reject AppleEvent sending, privileged task ports and named shared-memory creation. These are policy queries, not actual AppleEvents sent to a user application.
- A freshly compiled source host resolves and bundles a real TypeScript fixture under deny-default. An initial manual fixture used a noncanonical temporary path and failed its write grant; using the canonical path, as production SandboxPolicy does, passes.
- Strict all-target/all-feature sandbox Clippy passes. These are functional checks, not performance or Linux x64 qualification.

## Remaining inherited capability boundary

A separate harmless probe registered a parent-owned Mach send right, launched the child through sandbox-exec with deny-default, retrieved the inherited registered right and successfully sent a 24-byte message to that owned port. Policy does not revoke preexisting send rights. XNU explicitly inherits registered, exception, bootstrap and task-access ports in [ipc_task_init](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/ipc_tt.c). The trusted bootstrap must clear or otherwise reject inherited delegation authority before any untrusted entry executes. This leaf does not claim full code-only containment or arbitrary shell descendant containment.
