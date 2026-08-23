Rottweiler separates permission to read repository files from permission to
execute repository-controlled configuration.

## Project trust

Commands, skills, agents, workflows, modes, hooks, MCP servers, and plugins in a
project remain inert until their exact inventory is trusted.

```sh
rw trust status
rw trust grant
rw trust revoke
```

Changing the executable inventory invalidates the recorded decision. If the
inventory cannot be read completely, Rottweiler discards the partial result and
does not mint a trust fingerprint.

`--dangerously-trust` is for automation that establishes checkout identity
outside Rottweiler. It does not persist a trust decision.

## Permissions

- `strict` asks before operations that require approval.
- `auto-safe` automatically permits operations classified as safe.
- `yolo` removes interactive approval prompts.

None of these modes removes canonical workspace roots, project trust, or the
platform sandbox.

## Credentials

Configuration stores credential references, not values. API keys and OAuth
tokens do not enter config rendering, session logs, replay, exports, provider
catalog UI, or plugin processes. Provider plugins can request host-mediated
authenticated HTTP using an approved credential reference without receiving
the secret.

## Report a vulnerability

Do not open a public issue for a suspected sandbox escape, credential exposure,
signature failure, or permission bypass. Use the repository's
[private vulnerability reporting form](https://github.com/Boredphilosopher96/Rottweiler/security/advisories/new).
