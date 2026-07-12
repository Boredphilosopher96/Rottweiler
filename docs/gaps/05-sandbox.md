# 05 — Sandbox

## GAP-05-01 — macOS Seatbelt profile is `(allow default)` — a deny-list, not the allow-list the threat model assumes — **P1 [code]**

**Resolved within the recorded ADR (2026-07-12).** `05-SECURITY` deliberately specifies broad reads on macOS for compiler/toolchain compatibility, so the implementation does not silently replace that decision. It now adds hard read denials for Rottweiler, SSH, cloud, package-manager, GitHub/OpenCode/Codex, key, and token locations while retaining canonical workspace/scratch write restrictions; syscall acceptance remains green (`a897be5`).

`crates/rw-sandbox/src/lib.rs:751` builds the profile as:

```
(version 1) (allow default) <read_rule> (deny file-write* …) <network>
```

`(allow default)` means everything not explicitly denied is permitted:

- **Reads are unrestricted unless `read_roots` is set** — `read_rule` is empty when `policy.read_roots` is `None` (`:734,746`), so a sandboxed command can read `~/.ssh`, `~/.aws/credentials`, other repos. 05-SECURITY says "FS read broad" by intent — but combined with any network-granted command this is an exfiltration path, and threat-model item 1 is exactly "exfiltration of secrets via network tools."
- Everything else Seatbelt governs (exec, IPC, mach services, devices) is allowed by default.

**Fix.** Deny-by-default profile with explicit allows for workspace, scratch, and minimum system paths; if read stays broad, explicitly deny known secret locations (`~/.ssh`, `~/.aws`, `~/.rottweiler`, cloud-cred dirs).

## GAP-05-02 — Network egress pinning is by loopback port, not the specific proxy socket — **P2 [code]**

**Resolved (2026-07-12).** A proxy-granted macOS launch now fails closed unless the requested port is owned by a live `SupervisedEgressProxy` instance in the engine process. Dropping that owner removes the launch authority before the listener is released; arbitrary or reused host ports are rejected (`f7c7c3f`). Linux retains the stronger private Unix-relay/network-namespace route.

The `PolicyProxy` profile (`:742`) allows outbound to `localhost:{port}` — any process on that port, not exclusively the egress proxy; exactly the caveat 05-SECURITY Layer 2 called out. Prefer a unix-domain socket to the proxy or verify listener identity.

## GAP-05-03 — SSRF guard for private/link-local IPs is implemented — **[code, positive]**

`rw-sandbox/src/lib.rs:371-407` rejects private/loopback/link-local (incl. unicast link-local), with an explicit `169.254.169.254` metadata test at `:1355`. The round-2 SSRF hardening is present and correct.
