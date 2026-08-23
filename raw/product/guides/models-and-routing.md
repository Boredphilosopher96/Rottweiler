The engine addresses model aliases rather than spreading provider-specific IDs
through sessions and clients.

## Define an alias

```toml
[models]
default = "coding"

[models.aliases]
coding = [
  "anthropic/<primary-model>",
  "openai/<fallback-model>",
]
```

Candidates are ordered. Routing applies configured availability, capabilities,
limits, and fallback policy without changing the provider-neutral session
format.

## Inspect the catalog

```sh
rw models list --refresh
rw models show coding
```

The catalog can expose bounded display metadata and sanitized auth or
reachability state. Provider endpoints, credential references and values,
proxy details, wire errors, and routing internals stay inside the Rust engine.

Model capabilities come from the same catalog record that owns context limits
and pricing. When a model declares image input, the composer exposes image
paste and accepts image attachments. When it does not, those controls stay
hidden and image attachments are rejected before a provider request is sent.

## Pricing

User configuration can declare per-model USD API rates. Pricing precedence is
whole-record: user configuration, then authenticated provider discovery, then
models.dev. Fields are not blended from multiple sources. Subscription and
Copilot routes use quota or credit accounting and reject dollar pricing rather
than appearing as free API routes.
