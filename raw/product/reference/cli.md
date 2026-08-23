`rw` is the only public executable. Run `rw <command> --help` for the exact
flags accepted by the installed version.

## Top-level workflows

| Invocation | Purpose |
|---|---|
| `rw` | Start the supervised terminal application. |
| `rw -p <prompt>` | Run one prompt without the TUI. |
| `rw --remote <host>` | Run the local TUI against an engine reached over SSH. |
| `rw serve` | Start the engine client server. |
| `rw prompt dump` | Inspect the assembled prompt. |

## Global session and policy flags

| Flag | Meaning |
|---|---|
| `--permission-mode <strict\|auto-safe\|yolo>` | Select the non-interactive permission policy. |
| `--max-turns <count>` | Bound provider iterations in one user turn. |
| `--model <alias>` | Override the provider-neutral model alias. |
| `--add-dir <path>` | Add a canonical workspace root. Repeatable. |
| `--resume <session>` | Resume one exact durable session. |
| `--continue` | Continue the most recently updated session. |
| `--dangerously-trust` | Enable project execution for this run without persisting trust. |
| `--output-format <text\|json\|stream-json>` | Select headless output. |

## Configuration, models, and authentication

```text
rw config check
rw models list [--refresh]
rw models show <model-or-alias>
rw models refresh
rw auth login <provider>
rw auth set-key <provider>
```

## Trust and extensions

```text
rw trust status
rw trust grant
rw trust revoke
rw plugin scaffold --lang ts --name <name> <path>
rw plugin check <path> --allow-exec
rw plugin dev <path> --session <id|current> --allow-dev-exec
rw plugin status
rw plugin approve <plugin>
rw plugin revoke <plugin>
rw extension registry list
rw extension registry install <source>
rw extension status
rw extension enable <name>
rw extension disable <name>
```

## MCP

```text
rw mcp login <server>
rw mcp-server stdio [--workspace <path>]
```

## Durable work and diagnostics

```text
rw sessions list [--limit <count>]
rw sessions search <query> [--limit <count>]
rw replay <session>
rw export <session> [--format markdown|html|json]
rw stats
rw import <claude|opencode|pi> --source-root <path> --dry-run
rw doctor [--network]
rw upgrade [--channel stable|beta]
```

Aliases are not part of the CLI contract. The command tree has one spelling for
each operation.

`plugin check` validates manifest/package identity, then runs the plugin's
declared `typecheck` and `test` scripts. It executes local code only after the
explicit `--allow-exec` grant and does not attach the plugin to a live session.
