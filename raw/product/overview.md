- **One engine, every workflow:** The TUI, one-shot prompts, remote clients, and MCP use the same durable session engine.
- **Safe where it matters:** Project trust, explicit permissions, sandboxing, and credential isolation are engine boundaries—not UI promises.
- **Built to be extended:** Commands, skills, modes, MCP, hooks, WASM, and RPC plugins meet documented contracts.

## Productive in a terminal. Reliable in automation.

Rottweiler combines a compiled OpenTUI frontend with a headless Rust engine.
Interactive work feels immediate, while scripts get structured output, bounded
turns, explicit permission modes, and deterministic replay. Sessions survive
process exits and remain searchable, resumable, forkable, rewindable, and
exportable.

### Start in five minutes
Install the complete bundle, validate it, configure one provider, and run
your first task. [Follow the quick start](/Rottweiler/docs/installation/).

### Automate a real task
Use JSON or streaming JSON, set a turn budget, and select an explicit
permission policy. [Build a headless workflow](/Rottweiler/docs/tutorials/automate-a-task/).

### Give an agent the docs
Point an agent at [`llms.txt`](/Rottweiler/llms.txt), the complete
[`llms-full.txt`](/Rottweiler/llms-full.txt), or the structured
[`docs-index.json`](/Rottweiler/docs-index.json).

## One product, one source of truth

The documentation, copyable examples, raw Markdown, search index, and API
artifacts are built together. Release targets and schemas are projected from
their machine-owned sources instead of being redefined in prose.

## Open contracts, not hidden doors

Rottweiler publishes JSON Schemas and TypeScript projections for its Client
API, durable session log, and Plugin API. Built-in execution still
goes through the same registries and permission boundaries exposed to
extensions. Download the [machine-readable artifacts](/Rottweiler/docs/reference/plugin-api/)
or browse the [architecture](/Rottweiler/docs/concepts/architecture/).
