Rottweiler persists session events so the UI process is not the owner of your
work.

## Start a clean conversation

Press `Ctrl+N` (from the composer or the Sessions screen), or enter:

```text
/new
```

The engine creates the session and returns its identity before the TUI switches.
The previous conversation remains durable. Press `Ctrl+S` (or enter `/resume`)
to open the Sessions screen. Each row shows when the session was last active and
how many turns it has; the detail pane shows its first prompt, workspace, model,
and USD cost when every charge was priced. Enter resumes the selected session
directly, and the footer lists `Ctrl+R` rename, `Ctrl+X` export, and `Ctrl+N` new.

## Find the session

```sh
rw sessions list --limit 20
rw sessions search "release audit" --limit 10
```

## Continue it

Continue the most recently updated session:

```sh
rw --continue
```

Or choose the exact ID printed by `sessions list`:

```sh
rw --resume <session-id>
```

## Replay the history

```sh
rw replay <session-id>
```

Replay reconstructs behavior from the durable event stream. It is useful for
debugging, reviewing tool activity, and checking what a client displayed.

## Export a shareable transcript

```sh
rw export <session-id> --format markdown --output review.md
rw export <session-id> --format html --output review.html
rw export <session-id> --format json --output review.json
```

Exports pass through the transcript redactor. Still inspect an export before
sharing it; repository content can itself contain sensitive information.
