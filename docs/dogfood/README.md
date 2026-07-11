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

For a tag release, encode the reviewed external ledger as canonical base64 in
the protected `release` environment secret
`ROTTWEILER_DOGFOOD_LEDGER_B64`. The workflow materializes it as a private
temporary file, validates a window ending on the release runner's current UTC
date, retains only the non-secret gate result, and deletes the ledger before
signing. Missing or stale evidence blocks publication.
