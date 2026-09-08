"""Physical cleanup budgets, independent of measured workload deadlines."""
from __future__ import annotations

import time

# A local owner spends at most fifteen seconds retiring physical work. Nested
# owners receive ten cooperative seconds; five remain for force/reap/absence.
RETIREMENT_SECONDS = 15.0
COOPERATIVE_SECONDS = 10.0
# A daemon resource owner completes its whole cancel/reply/remove/verify chain
# inside eight seconds, leaving room for its outer owner to observe closure.
RESOURCE_CLEANUP_SECONDS = 8.0


def remaining(deadline: float) -> float:
    return max(0.0, deadline - time.monotonic())
