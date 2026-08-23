# Documentation map

Rottweiler has one public documentation owner: `packages/docs-site`. It builds
the human site, search index, raw Markdown, `llms.txt`, `llms-full.txt`, and
`docs-index.json` together.

The Markdown beside this file is maintainer documentation:

- `01-FEATURES.md` records product intent. It is not shipped-status evidence.
- `02-ARCHITECTURE.md` records current and intended system boundaries.
- `03-DECISIONS.md` is the architecture decision log.
- `04-EXTENSIBILITY.md` and `05-SECURITY.md` are deep maintainer contracts.
- `06-ROADMAP.md` preserves the historical implementation sequence.
- `07-VERIFICATION.md` records evidence tiers and repository gates.
- `design/` contains focused architecture records.
- `dogfood/` documents the protected v1 evidence contract.
- `gaps/` and `reviews/` are dated historical audits, not current product docs.
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
