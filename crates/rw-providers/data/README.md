# Bundled model metadata

`models-dev.json` is an offline fallback for limits, capabilities, and pricing,
extracted from <https://models.dev/api.json>. It never grants model availability;
authenticated provider discovery remains authoritative. User configuration and
provider metadata retain their existing precedence. The snapshot stores its UTC
capture date and the SHA-256 of the complete upstream response.

Regenerate from a downloaded response with
`python3 scripts/update-bundled-models.py /path/to/api.json --date YYYY-MM-DD`.
The extractor preserves only fields consumed by the shared Rust converter and
includes OpenAI, Anthropic, GitHub Copilot, Google, and OpenRouter namespaces.
Missing price records are omitted using the same priced-model contract as refresh.
New upstream namespaces can be added to the extractor when an adapter needs them.

Live model discovery refreshes missing/day-old metadata through the existing
proxy-aware refresh owner with a three-second download deadline and an hourly retry bound.
Startup uses local data only. An offline refresh failure retains the installed or
bundled table. Catalog refresh does not rewrite a running turn's accounting.
