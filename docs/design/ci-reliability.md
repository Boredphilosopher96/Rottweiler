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

Protected work needs an external hosted coordinator to enforce queue deadlines.
It dispatches independent platform workers, correlates exact candidates, and
cancels only its owned child on queue expiry. An in-job timeout cannot limit
waiting for the runner. Capacity provisioning and actual eight-hour runs remain
qualification obligations; adding a coordinator cannot satisfy them.

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

The initial guard checks current idle eligibility; it is not a reservation and
does not solve disappearance after admission. A future external queue controller
must use a bounded dispatch/watch phase plus completion-triggered collection:
a GitHub-hosted controller cannot itself wait eight hours because hosted jobs
have a shorter maximum duration. Full queue lifecycle and independent platform
soak dependencies remain pending; the current guard prevents the observed
zero-capacity queue stall. Candidate artifacts now survive fourteen days.

Dependabot's native Bun ecosystem maintains `bun.lock`; see the
[GitHub options reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference).
The previous npm configuration did not produce matching Bun locks. Frozen
installation remains required.
