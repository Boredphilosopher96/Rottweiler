Rottweiler can consume configured MCP servers and can expose its own guarded
tool surface over standard input/output.

## Use Rottweiler as an MCP client

Declare servers in the user or trusted project MCP configuration, then inspect
them in the TUI with `/mcp`. HTTP servers that use Authorization Code with PKCE
can start login from the CLI:

```sh
rw mcp login <server-name>
```

Project MCP configuration participates in the project extension inventory. It
does not execute until the exact inventory is trusted.

## Use Rottweiler as an MCP server

Expose one workspace over stdio:

```sh
rw mcp-server stdio --workspace /absolute/path/to/project
```

Configure that command in the client that will launch Rottweiler. The MCP
connection does not bypass Rottweiler's workspace, permission, trust, or
sandbox boundaries.

## Verify the boundary

Run `rw trust status` in the workspace and use `rw config check` to inspect the
effective policy before connecting an automated client.
