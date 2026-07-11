# v1.0 self-hosting evidence

The v1.0 release gate requires 14 consecutive UTC days of Rottweiler-driven
development with no P0 data-loss, corruption, or hang incident. Evidence is an
append-only JSONL ledger maintained outside the source tree until the release
candidate window begins. Each daily record has exactly:

```json
{"date":"2026-07-10","commit":"0123456789abcdef","session_ids":["session-id"],"p0_incidents":0}
```

Validate a candidate ledger with:

```sh
python3 scripts/check-dogfood-gate.py /path/to/dogfood-ledger.jsonl
```

The validator fails closed on gaps, duplicates, P0s, stale windows, malformed
or replaced files, oversized input, missing session evidence, and unknown
fields. A release workflow must pass this validator; a single development run
must never manufacture the two-week temporal evidence.
