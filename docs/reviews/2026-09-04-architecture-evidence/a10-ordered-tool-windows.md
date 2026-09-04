# Ordered tool execution windows

The engine now runs contiguous read-only calls in an ordered window of at most
16 calls. A mutating call waits until all earlier results are published and
runs alone; the next read window opens after that mutation, its checkpoint and
its post-tool processing finish. The old all-parallel/all-serial branches are
deleted.

The window includes completed later calls waiting behind an earlier call.
Limiting only active tasks would allow the scheduler to accumulate an unbounded
completed tail while the first call stalls. Output and subagent event order
continue to advance only with the next published result. Execution tasks retain
ownership through the existing effect-settlement and checkpoint path.

The production actor tests exercise two read runs around a write, reverse
completion with ordered publication, 48 calls behind a blocked first call,
cancellation without starting the remaining calls, saturated output, and
checkpoint cleanup. All 311 core library tests pass.

This is a partial A10 checkpoint. The window bounds each batch's active tasks and
pending ordered completions. It does not bound aggregate concurrency across
sessions, the total provider call list, all prepared argument bytes, or the final
collected result list. Shared process/network/blocking-I/O/CPU admission and
bounded call-list admission remain necessary. Finer write parallelism is not
introduced without a verified resource-conflict contract.
