# Runtime cleanup retains transferred resources

Integration review found that timing out the runtime resource cleanup future dropped that future. MCP shutdown can transfer clients out of its registry before awaiting them, so retaining only the service Arc did not preserve those actual owners. The runtime now owns and continues polling the exact cleanup future after its 30-second proof deadline. Failure remains sticky while cleanup continues. Each service catches its own panic, so an MCP failure cannot skip plugin cleanup.

Three regression tests pass: a simulated service transfers its actual resource out of a registry, exceeds the deadline, and still completes cleanup after release; one service panic does not skip the other; dropping the last resource handle requests owned cleanup.

Combined verification includes the host closure, session-resource, native history and six stable runtime domain changes. With pinned Bun1.3.14 on macOS ARM64, all369 core unit tests and264 runtime unit tests passed, with one existing ignored test in each crate. Strict all-target/all-feature core and runtime Clippy passed. The actual plugin-child signal and proxy-settlement regressions are included. Raw logs are retained alongside this report.

The stable domain extraction preserves every function body and separates metadata, prompt shapes, initial project memory, web fetching, session selection/leases and accounting projections. The large composition and test sections still require semantic extraction.

Remaining MCP ownership work belongs to A29: manager connect/catalog/disable/shutdown paths and RmcpClient::close can transfer clients, service handles or child handles before awaits. Those operations need owned lifecycle tasks and failed-owner retention at their own boundary. This runtime wrapper fix does not claim to close those lower-level gaps.
