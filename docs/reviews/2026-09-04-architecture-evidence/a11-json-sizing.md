# JSON estimation allocation checkpoint

This is a partial A11 improvement. The previous token estimator cloned and
sorted every JSON value, then allocated its serialized representation to obtain
the byte count. Object key order cannot change that count. The estimator now
sends the same serializer's output to a byte counter without retaining it.
Canonical serialization for actual prefix hashes remains unchanged.

The counter uses constant auxiliary storage, plus the serializer's recursive
traversal stack. It still visits all input bytes. It does not cache estimates,
make context assembly incremental, or eliminate image-to-JSON conversion.

All 43 context unit tests and the estimator accuracy integration test pass.
Differential checks compare against the old canonical serialization across
nested generated JSON, numeric boundaries, Unicode, escaping, and a large
string. All-target context clippy and format checks pass.

Reproduce the diagnostic comparison with:

```sh
cargo run --release --locked -p rw-context --example json_sizing
```

The harness alternates the two implementations, performs one warmup and retains
100 samples for each size. On the shared macOS ARM64 development host, median
times for 1 KiB / 1 MiB / 8 MiB text values changed from
0.959 / 427.750 / 3267.583 microseconds to
0.417 / 352.667 / 2835.292 microseconds. Tail timings varied under background
load, including a higher counted p99 for the 1 MiB fixture. These are diagnostic
measurements, not controlled qualification or a general p99 improvement claim.
The companion JSON retains all samples.

Remaining A11 work includes shared immutable context blocks, registry and item
revision keys, incremental token totals and canonical-prefix caching, request
equivalence across invalidations, and controlled whole-assembly allocation and
latency measurement.
