Start with local evidence:

```sh
rw --version
rw config check
rw doctor
rw trust status
```

`rw doctor` exits unsuccessfully when no provider is configured or the default
model cannot resolve to a configured route. A fresh installation should not be
reported as ready until both checks pass.

## Provider or credential problems

Network checks are opt-in:

```sh
rw doctor --network
rw models list --refresh
```

Use `rw auth set-key <provider>` for API-key routes and `rw auth login
<provider>` for configured browser or device flows. Do not put secret values in
TOML to make a test pass.

## An extension does not load

Check project trust first. Invalid or unreadable artifacts are skipped with a
diagnostic; the rest of the application can still start. Re-run `rw trust
status` after any executable extension file changes.

For a TypeScript plugin, validate its identity and run its declared checks
without attaching it to a session:

```sh
rw plugin check ./path/to/plugin --allow-exec
```

The execution flag is required because the command runs package scripts.

## Another process is using this workspace

Only one engine writes a workspace at a time. A second process reports that it
is waiting, permits `Ctrl+C`, and gives up after a bounded interval instead of
appearing to hang. To start another conversation in the attached engine, press
`Ctrl+N` or enter `/new`.

## The runtime path is too long

Rottweiler normally keeps runtime files below its storage root. When that path
cannot fit the platform's Unix-socket limit, it automatically uses a private,
hashed directory below the system temporary directory. Session data remains in
the configured storage root; only short-lived runtime files move.

## The application starts incompletely

Verify that you installed the complete release bundle. A standalone Rust binary
does not include the terminal executable, native renderer, WASM host, and plugin
host. Reinstall using [Installation](../installation.md).

## Upgrade or rollback

```sh
rw upgrade --channel stable
rw upgrade --rollback
```

Use `--channel beta` only when you intentionally accept a prerelease update
channel. Signed metadata prevents version rollback unless rollback is explicit.
