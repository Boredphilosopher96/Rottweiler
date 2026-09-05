# Bounded local client timing

`ROTTWEILER_CLIENT_TIMINGS=1 rw` enables one diagnostics owner in the TUI process.
On renderer destruction it writes one `[rw-client-timings]` JSON record to stderr.
The ordinary launcher inherits this opt-in variable. Disabled startup does not
load the diagnostics module; disabled instrumentation performs no timing-clock
reads or per-observation allocations.

The owner retains ten fixed stages with fifteen fixed histogram buckets each,
plus saturating numeric counts, units, total and maximum milliseconds. It retains
no observations, event payloads, session IDs, command names or document text.
Snapshot arrays are independent copies; a snapshot cannot mutate live counters.
Each renderer process reports independently, including a memory recycle.

## What the stages mean

| Stage | Measured boundary | Units |
| --- | --- | --- |
| event_decode | SSE JSON parsing and generated event validation | events |
| reply_decode | UTF-8 and JSON decode after the bounded body read | bytes |
| reply_validation | generated reply validation and correlation checks | replies |
| reducer | parent or active child state reduction | events |
| presentation_queue_age | oldest pending UI invalidation to binding start | batches |
| presentation | synchronous component binding | pending events |
| history_queue_age | oldest source invalidation to next read start | reads |
| history_admission | source-page validation and cache admission | semantic rows |
| history_update | native semantic row reconciliation | calls |
| history_layout | OpenTUI layout and stable-anchor restoration | layout calls |

Durations use a monotonic wall clock. Presentation/update/layout measurements
can nest and must not be added into a fabricated end-to-end total. These are
client stage timings, not terminal I/O or asynchronous Tree-sitter CPU accounting.
The existing M4 process-CPU/frame/input gates retain their separate statistics.

The production authenticated M9 test enables the owner on its actual transport
and native renderer, verifies all exercised stage counters, and asserts that a
source filename does not enter the report. A production PresentationController
regression injects 80 ms of queue delay and a 25 ms callback: those observations
land in separate buckets/stages. Another test takes 100,000 observations and
verifies fixed storage dimensions and independent snapshots.

## Local overhead evidence

Run `bun packages/tui/scripts/client-timings-probe.ts` with pinned Bun 1.3.14.
The script compares disabled/enabled diagnostics in alternating order, using
200 warmup and 400 measured iterations for each of eight trials. It exercises
the production bounded JSON response reader with a 256 KiB payload and the native
16-row history renderer. Mock Tree-sitter isolates the main-thread instrumentation
comparison; the existing full TUI gates still exercise real Tree-sitter.

The retained `a20-client-overhead.json` is a local macOS arm64 diagnostic, not a
new gate statistic. Mean response-reader process CPU was 0.252 ms disabled and
0.267 ms enabled (+0.015 ms); trial ranges overlapped. Mean native history frame
CPU was 0.775 ms disabled and 0.745 ms enabled, also within the trial spread. The
negative frame difference is measurement variability, not a speedup claim.

The native layout test also now waits for the final logical source row when
scrolling down. A physical window reaching its current bottom does not mean the
logical history has ended. The existing 500-wheel-step bound and source-content
assertions remain, matching the upward-scroll contract.

Final local verification: TypeScript typecheck; **560 TUI tests passed, 0 failed,
20 snapshots**, including unchanged M4 performance/transport gates and the real
Tree-sitter scrolling regression. The compiled bundle is **85,615,216 bytes**,
below the unchanged 100,000,000-byte budget. No Rust schema or journal changes
are part of this unit.
