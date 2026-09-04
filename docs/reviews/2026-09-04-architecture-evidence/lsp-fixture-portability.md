# LSP fixture portability

The integrated macOS test run failed while waiting for the process-tree
fixture's descendant PID. A focused repeat reproduced the failure. Temporarily
capturing the child's stderr exposed the cause: `/usr/bin/python3` invoked
Apple's developer-tool shim, which tried to create an `xcrun_db` cache outside
the sandbox's private writable scratch root. The test discarded stderr, so its
visible failure was only a two-second readiness deadline.

The teardown fixture now uses `/bin/sh` and `/bin/sleep`, announces the spawned
descendant over stdout, and cleans up on readiness failure. It still launches
through the production sandbox and verifies that the complete process group
disappears. The protocol fixture still needs Python; on macOS it resolves the
real interpreter with `xcrun --find python3` before entering the sandbox and
passes the script as an argument. Linux uses `/usr/bin/python3` directly.

All seven intelligence tests pass, including the sandbox write/network refusal
and process-group teardown tests. The production sandbox policy is unchanged.
The temporary stderr diagnostic edit was removed. This is local macOS evidence;
the next exact-head hosted run supplies the two-platform result.
