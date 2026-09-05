# Derived index lock settlement

A concurrent core-suite run reported `TranscriptIndexError::Busy` while reopening
a dropped projector. That error identifies the independent flock, not redb's
database-open error. The run did not retain a process/descriptor trace, so exact
attribution of that occurrence remains unproven.

A deterministic regression reproduced a concrete ownership flaw: duplicate the
owner's lock descriptor (the same shared open-file description inherited across
fork), drop the database owner, and immediately reopen its index. The old
close-only owner failed with `Busy`. CLOEXEC does not cover a forked child's
interval before exec closes the inherited descriptor.

`ExclusiveFileLock` now releases its flock explicitly on owner drop, retrying an
interrupted unlock. The database is declared before the lock guard, so database
shutdown remains protected. The same regression passes, and closing the old
inherited descriptor cannot release the replacement owner's independent lock.
The shared guard is available to the raw journal owner; its adoption is separate.

Verification on the private macOS target: the failing-before/passing-after
regression, full store suite (193 passed, two explicit performance probes ignored),
strict all-target store clippy, and the original repeated projector reopen test.
No sleep, retry budget, or performance threshold was added.
