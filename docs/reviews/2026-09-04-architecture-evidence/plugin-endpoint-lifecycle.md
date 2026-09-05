# Plugin endpoint boundary

The A28 endpoint leaf separates validated registration metadata from live RPC
connections. It follows ADR-035. Runtime dormant activation and removal of eager
session composition are still in progress; this leaf alone does not close A28.

All RPC tool, hook, command, provider and event adapters now take one
`Arc<dyn PluginEndpoint>`. Their old client/enforcer pair constructors were
removed. `PluginConnection::from_host` obtains both from an initialized host.
`ReadyPluginEndpoint` owns that host and rejects new connections after closure.
A configured dormant endpoint will implement the same required connection and
cleanup contracts while owning preparation and startup separately.

Permission metadata preserves the union of authority available to the shared
plugin process. The inert descriptor and the live enforcer use one derivation.
Metadata is not execution approval. The new regression checks that descriptor
queries and declaration validation never request a connection, while execution
requests one and still receives the approval rejection. It also checks that an
unsettled endpoint error reaches the tool's required cleanup result.

`PluginRpcClient::settle_effects` is now required. The two pure response fixtures
explicitly declare no outstanding effects. The production client retains its
process and admitted host-effect proof. No compatibility constructor or implicit
successful cleanup remains in this RPC client boundary.

Verification on macOS arm64 with Rust1.97.1 and pinned Bun1.3.14:

- `cargo test -p rw-ext`: 137 passed, including actual Rust/TypeScript tool, hook,
  provider, model catalog, authenticated HTTP, event and cancellation paths.
- `cargo clippy -p rw-ext --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all --check` and `git diff --check`: passed.

Raw terminal outputs are retained beside this report as
`a28-endpoint-ext-tests.txt` and `a28-endpoint-clippy.txt`. These are functional
boundary checks, not startup/RSS/performance or protected release qualification.
The runtime consumer migration is coordinated with the required actor close wave.
