# M4 direct-read protocol migration

The socket gate previously posted `list_commands`/`list_models`, waited for their
result on SSE, and decoded HTTP as a bare `CommandOutcome`. Both assumptions became
invalid when host reads moved to `CommandReply::Read`; the gate would wait for an
event that is deliberately no longer published.

The gate now measures two channels independently. Alternating command/model reads
receive a direct typed HTTP reply and validate its outcome, result kind, protocol,
authenticated client, request ID and applicable session ID. An observer resume of
the already loaded session produces the real `SessionsListed` SSE event used for
`uds_event_p99_us`. It also receives a typed control acknowledgement over HTTP.
Readiness happens outside the measured window and never waits for a query on SSE.

The existing nearest-rank p99 statistic, warmup/sample counts and strict 2 ms SSE
ceiling remain. Direct-read samples and their p99 are separate diagnostic evidence;
they do not replace the SSE metric or silently add an uncalibrated key to the strict
baseline metric map. Evidence records the command/event stimulus explicitly. The
observer operation is a changed SSE stimulus, so historical query-driven SSE
samples are not an exact workload match; this checkpoint does not recalibrate any
protected release baseline or claim release-runner qualification.

`python3 -m unittest discover -s scripts/tests -p 'test_m4*.py'` passes seven tests.
New tests prove no direct read waits on SSE, both channels produce separate samples,
wrong reply classes/foreign identities fail, and exactly 2 ms still fails the SSE
ceiling. A repository-wide Python search for command endpoints and query event
consumers found no other HTTP command consumer requiring migration.

A current debug `rw` built from the integrated `b4dcd5e` tree with a private target directory
ran the production socket gate against the local offline provider with 100 samples.
The final run recorded SSE p99 502 us and direct-read p99 373 us; all requests used
the actual authenticated Unix socket and event stream. Raw samples are in
`a04-m4-direct-read-local.json`. These are local correctness/performance diagnostics,
not release-optimized or hosted benchmark claims.

The socket helper moved to `scripts/m4_socket_latency.py` by responsibility, leaving
the release gate at 1,286 lines. The full source-size gate still reports the other
assigned migration owners; this checkpoint does not claim those files are fixed.
