# Journal timing attribution

The rw_performance trace target now includes journal.append, journal.serialize, journal.write, journal.sync and journal.page. Append and page spans include the session ID, sequence bounds and byte/event counters. They do not record payloads or storage paths. Existing CLI close-span formatting reports the timing when RUST_LOG enables this target.

A production-path test injects a 20 ms synchronization delay and verifies that journal.sync owns that duration beneath journal.append. It also checks append/page counters, target filtering and absence of the sentinel payload and workspace path. All 206 store tests passed, with two existing ignored qualification tests. The filtered attribution test and strict all-target/all-feature Clippy passed again after aligning the target with core tracing.

The sparse segment catalog moved to its own module and retained its growth/prefix regression. Journal code remains below the 1,500-line limit. No behavior or format adapter was added.

## Local cost experiment

Run `python3 scripts/measure-journal-tracing.py --output PATH`. The script builds the production journal example in release mode both with ordinary tracing and with tracing/max_level_off. Both binaries are built before measurements. Three alternating rounds retain 63 samples for each configuration, with 200 repetitions per empty-append and page sample. Temporary executable copies are deleted automatically; Cargo output reuses the checkout target.

Median nanoseconds per operation from the retained run:

| Operation | Compiled out | Disabled | Enabled, formatted sink |
| --- | ---: | ---: | ---: |
| Empty append | 366 | 374 | 3265 |
| Small history page | 13595 | 13697 | 15698 |
| Single durable append | 113625 | 122334 | 148500 |

These are local diagnostics from a shared macOS ARM64 host. Small differences are not a controlled causal estimate; durable synchronization especially depends on storage and host activity. Enabled measurements include close-event formatting to io::sink and exclude terminal/disk logging cost. No provider or model work runs in this experiment. Raw samples, source hashes, executable hashes, Rust version and platform are in [journal-tracing-cost.json](journal-tracing-cost.json).

A20 remains open for UI stages and the remaining engine admission/hook coverage and controlled platform qualification.
