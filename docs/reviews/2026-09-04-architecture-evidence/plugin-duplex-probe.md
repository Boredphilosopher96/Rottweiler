# Plugin duplex RPC probe

Run from the repository root:

```sh
bun run docs/reviews/2026-09-04-architecture-evidence/plugin-duplex-probe.ts
```

The probe uses the production SDK `PluginServer.serve` loop and an in-memory
transport. It does not access the network, load credentials, or start an external
plugin. The transport immediately queues a valid HTTP response when the SDK
requests one during `provider/models`.

Observed on 2026-09-04 with Bun 1.4.0: request 2 returns error `-32004`,
`plugin handler timed out`, followed by the `REPRODUCED` line. The deadline is
shortened to 80 ms; the production default is 5 seconds. This is a dependency
cycle, not an assertion about execution speed: the input loop waits for the
catalog handler, and the catalog handler waits for an HTTP response that the
input loop must consume.

Relevant production code: `packages/plugin-sdk/src/server.ts:509` awaits
`handleLine`; line 697 dispatches and awaits the provider catalog handler;
line 933 awaits its host HTTP response. Only `provider/complete` is detached
from the input pump at line 567.

After fixing the transport, the expected response for request 2 is
`{"jsonrpc":"2.0","id":2,"result":{"models":[]}}`; update the probe to assert
that outcome. A general regression should also cover cancellation and HTTP
response processing while unrelated event/tool handlers are awaiting work.
