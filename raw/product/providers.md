Rottweiler keeps provider wire formats behind one internal message model. A
session selects a model alias; routing resolves that alias to an ordered list
of provider/model candidates.

## Adapter kinds

User configuration supports these provider kinds:

- `anthropic`
- `openai`
- `openai_chat`
- `openai_codex`
- `github_copilot`
- `openai_compatible`
- `openai_compatible_responses`

Use the dedicated kind when one exists. The compatible kinds are typed gateway
surfaces, not arbitrary wire dialects.

## Credentials

Reference credentials from configuration and enter their values through
Rottweiler:

```toml
[providers.work]
kind = "openai_compatible_responses"
base_url = "https://api.example.com/v1"
api_key_credential = "providers.work.api_key"
```

```sh
rw auth set-key work
```

ChatGPT subscription and GitHub Copilot credentials remain isolated from API
key providers. Rottweiler does not copy credentials from another developer
tool.

## Compatible gateways

The gateway surface supports static `headers`, credential-backed
`header_credentials`, bearer/custom-header/no primary auth, `extra_query`,
`extra_body`, a `{model}` `path_template`, model ID remapping, and user-declared
pricing. It rejects reserved headers, duplicate authentication, query-string
credentials, and subscription transport overrides.

Provider definitions are user-scoped. Project configuration cannot replace
them, even after the project is trusted.

## Inspect before running

```sh
rw config check
rw models list --refresh
rw models show <alias-or-model>
```

`config check` prints the effective value and provenance of configuration
without rendering credential values.
