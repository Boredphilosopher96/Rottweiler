# Documentation map

Rottweiler has one public documentation owner: `packages/docs-site`. It builds
the human site, search index, raw Markdown, `llms.txt`, `llms-full.txt`, and
`docs-index.json` together.

The Markdown beside this file is maintainer documentation:

- `01-FEATURES.md` defines product requirements and behavior.
- `02-ARCHITECTURE.md` defines system boundaries and ownership.
- `03-DECISIONS.md` explains design decisions and rationale.
- `04-EXTENSIBILITY.md` and `05-SECURITY.md` are deep maintainer contracts.
- `07-VERIFICATION.md` records evidence tiers and repository gates.
- `design/` contains focused architecture records.
- `dogfood/` documents the protected v1 evidence contract.
- `assets/` contains the source artwork used by the README and docs build.

Vendored specifications under `crates/rw-context/spec/` and Markdown inside
test fixtures preserve upstream or fixture bytes. The first-party documentation
checker excludes those files from editorial claims while still protecting the
generated contracts that consume them.

Run the audit with:

```sh
python3 scripts/check-documentation.py
bun run --cwd packages/docs-site check
bun run --cwd packages/docs-site build
```
