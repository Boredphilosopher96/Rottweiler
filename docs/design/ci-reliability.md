# CI reliability ownership

Accepted September 4, 2026 for C01-C08 in the mission architecture review.

Workflow YAML owns jobs, dependencies, permissions and runners. The required
aggregate lists every other ordinary CI job in `needs`, runs with `always()`,
and accepts only success. A semantic YAML check prevents omitted jobs, path
filters and missing triggers. Optional qualification lives in separate workflows.

`contracts/package-inventory.json` owns real package membership and explicit
fixture exclusions. Package manifests own check commands and file dependencies;
Cargo owns workspace and fuzz target membership. Toolchain native files keep
owning versions. Package installation, matrix generation, toolchain coverage and
Dependabot projections consume the inventory. No second job graph is introduced.

Build and measurement identities include source, lockfiles, target, toolchain,
profile and provenance. macOS startup timing retains measurement-host builds.
Functional acceptance may reuse verified immutable artifacts. Budgets stay in
existing release/performance owners. Reports preserve nonzero process status and
bounded diagnostics; a failure category is never permission to pass a gate.

Nightly dispatches an independent protected workflow for each platform after
that platform's build succeeds. Each worker has a hosted queue watcher beside
the native workload. The watcher observes at most fifteen minutes, then records
nonexecution, uploads evidence, and cancels only its own worker workflow.
Checkout, observation, upload and cancellation have separate step deadlines
inside the watcher job's larger deadline. A bootstrap failure also reaches the
owned cleanup step. Once native execution starts, the watcher exits; it does not
wait eight hours on a hosted runner. Qualification requires validation, watcher
and workload success. A successful dispatch means pending, never qualified.

## Design comparison

Candidate A proposed a typed Python gate inventory with generated workflow
regions. Candidate B kept the job graph in GitHub YAML and introduced only a
checked package inventory. Both preserve limits and require a separate protected
queue owner. B is the base: permissions and dependency edges remain visible in
one executable owner. From A, retain narrow generated package projections and
reuse the existing candidate identity owner. Reject a second handwritten Gate
DAG and full workflow generation. The cross-review agreed that B has simpler
ownership, while both require correlation/cancellation and missing-result tests.

## Verification

Exercise aggregate success, failure, cancellation, skip and omitted-result cases.
Mutate package inventory, manifests and workflow dependencies in temporary
fixtures to prove drift is detected. Run each package independently with frozen
installation. Compile every fuzz target before scheduled campaigns. Retain
source-qualified results and actual hosted outcomes in the remediation ledger.

## Current operational limits

The September 4 live `gh` inventory has zero repository runners. The user
authorized checking GitHub and leaving provisioning there if none exist. No
machine or credential is assumed. Protected capacity needs a repository
administration-read token named `ROTTWEILER_RUNNER_READ_TOKEN` where the workflow
token cannot list runners. Missing permission or capacity fails explicitly and
retains a report. Hosted performance jobs do not depend on private soak capacity.

The admission guard checks current idle eligibility, not a reservation. The
worker watcher handles disappearance after admission. This mechanism still
requires GitHub-hosted capacity for the watcher and working cancellation APIs;
an accepted cancellation request does not prove terminal cancellation. These
are observable infrastructure failures, not performance passes.

Artifact names include their producer attempt. Failed-job reruns discover the
latest producer job across attempts: an unchanged successful producer can be
reused, but a newer failed producer cannot be bypassed. Source SHA, repository,
main-branch event, workflow identity, producer outcome and artifact expiry are
validated before private execution. Every engine/TUI file, including native
sidecars, is checked against a bundle manifest bound to source and platform.
The metadata and all eight-hour results remain on the worker run; its URL is
recoverable from the dispatch report's run ID.

Dispatch uses the [versioned workflow API](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event),
which returns the created run ID. An uncertain response is correlated using an
unguessable title component; a nearby run is never selected for cancellation.
The worker owns its own queue deadline even if the dispatch response is lost.
[Cancellation](https://docs.github.com/en/rest/actions/workflow-runs#force-cancel-a-workflow-run)
is isolated from package, fuzz, performance and other-platform jobs.

Soaks emit a metadata-only checkpoint to the streamed GitHub job log each minute
as well as atomic local reports. This preserves another observation path if a
runner disappears before artifact upload; it does not replace hosted fault
injection for loss-of-runner retention. No terminal or transcript content is
included in those heartbeat records. v1 release soak queue ownership and
cross-platform qualification aggregation still need the corresponding release
integration; nightly dispatch alone does not satisfy release qualification.

Dependabot's native Bun ecosystem maintains `bun.lock`; see the
[GitHub options reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference).
The previous npm configuration did not produce matching Bun locks. Frozen
installation remains required.
