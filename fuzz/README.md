# Rottweiler fuzz targets

These independent `cargo-fuzz` targets exercise the four untrusted parsing
boundaries required by the v1.0 gate:

- `config_parser`: bounded TOML configuration parsing.
- `toon_decoder`: bounded TOON structured-response decoding.
- `plugin_rpc`: incremental, chunked plugin RPC framing and buffer limits.
- `event_log`: bounded session JSONL recovery and validation.

Run one target with nightly Rust:

```sh
cargo +nightly fuzz run plugin_rpc -- -max_total_time=60 -rss_limit_mb=2048
```

The scheduled hardening workflow runs every target in an isolated job. Crash
artifacts remain local/CI artifacts; corpora may be committed only after they
are minimized and contain no credentials or user data.
