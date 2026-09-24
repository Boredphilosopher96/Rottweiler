---
title: Models and routing
description: Build provider-neutral model aliases, fallback chains, capabilities, pricing, and inspectable routing decisions.
sidebar:
  order: 2
---

The engine addresses model aliases rather than spreading provider-specific IDs
through sessions and clients.

## Choose a model in the TUI

Connecting a provider opens its models. If no model is selected, Rottweiler
selects an available tool-capable model from that provider's catalog. Connecting
another provider preserves your selection. `/models` changes it explicitly.
The first concrete selection becomes your user default when none is configured;
subsequent choices are remembered per workspace. Cached lists are labeled.

Context limits have an offline models.dev fallback. Opening model discovery
attempts a bounded refresh of missing or day-old metadata; a failed refresh keeps
local metadata. Provider discovery still decides which models are available.

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

### Compatible endpoints

Choose **Connect compatible endpoint…** in `/providers` to add a user-scoped
Chat Completions or Responses endpoint. Supply its full inference URL, for
example `https://gateway.example/v1/chat/completions`, and a unique local name.
API keys are entered separately and stored through the credential manager;
they never enter configuration commands or session history. Unauthenticated
connections are restricted to explicit loopback endpoints.

The optional initial model creates a `<provider>-model` alias without changing
an existing default or active session. Local servers that return 404, 405, or
501 for `/models` can use this configured route. A working live catalog remains
authoritative: models it omits cannot be selected, and authentication/network
failures do not enable the static fallback. Remote endpoints must expose their
model catalog. After connecting, choose a model in the model picker to make it
the active selection and save its default.
