# Native plugin shutdown settlement

Hosted CI run 33929264078 at bd631cadf819a68a9eb86e88ff0f43acb1fb898e
passed every workload except Test (macos-15). Its sandboxed sibling-read refusal
fixture failed during shutdown with `Operation not permitted (os error 1)`.
The aggregate correctly failed too.

Shutdown had its own cleanup path: it returned a kill-attempt error even when
reaping succeeded, and graceful exit only checked the process leader. Shutdown
now enters the existing owned request-termination path and marks completion
only after process effects and admitted host effects settle. A failed kill
attempt is diagnostic; successful settlement remains mandatory. The owned
cleanup task survives a caller's shutdown deadline.

The focused regression checks both a failed kill with successful settlement and
a failed kill whose settlement remains blocked beyond the API deadline. The
latter must fail shutdown, retain incomplete state, and allow a later call to
observe the original cleanup task's completion. Four shutdown tests and
all-target rw-ext clippy passed. The native macOS sandbox fixture passed once
through Cargo and in ten consecutive direct test-binary repetitions.

This fixes shutdown ownership; it does not claim containment of deliberately
escaped native descendants or eight-hour soak qualification.
