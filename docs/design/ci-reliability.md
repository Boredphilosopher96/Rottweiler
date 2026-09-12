# CI ownership

Workflow YAML owns jobs, dependencies, permissions, and runners. The required
aggregate lists every ordinary CI job in `needs`, runs with `always()`, and
accepts only success. Semantic checks reject omitted jobs, path filters, missing
triggers, and missing native build owners. Protected qualification runs in
separate workflows.

`contracts/package-inventory.json` owns package membership, source checks, native
product membership, and fixture exclusions. Package manifests own executable
commands and local dependencies. Cargo owns workspace and fuzz target membership.
The native toolchain files own versions. Package installation, matrix generation,
and Dependabot coverage consume these owners. Every local package dependency is
installed and built before its consumer's frozen installation.

## Native products

`scripts/build-native-candidate.py` builds the engine, WASM helper, TypeScript
plugin host, TUI, native renderer, and SDK. It enforces the component budgets and
archive contract before publishing an immutable candidate directory. Separate
Cargo invocations prevent helper features from entering the public engine.
Native executable compilation belongs to this job; package jobs independently
validate source types and behavior on both operating systems.

The candidate receipt binds the source commit and working-tree content, compiler
and Bun identities, native target, release profile, build configuration, and each
component's byte count and checksum. Configuration values that can affect the
binary, including embedded update trust roots, are represented by fingerprints.
The archive and staged components must contain identical bytes.

Publication is atomic and serialized per output directory. A completed candidate
is reused only after verifying its identity and all files. Each worktree reuses
its own Cargo target across checks. Acceptance gates require an identified
candidate; verification failures stop the gate. The native product owner also
constructs release archives and verifies their extracted entrypoints.

Headless timing uses a private executable copy checked against the candidate's
checksum. Linux native gates consume the same uploaded candidate. macOS startup
measurements build on the measurement host to control executable provenance;
all gates in that job use that candidate. The Linux sandbox container is a
separate build environment. Its engine and test fixture are built separately.
Raw samples retain the candidate identity and measurement-host information.
Performance ceilings and sampling policy belong to the release and performance
contracts.

## Protected workloads

Nightly dispatches a protected workflow for each platform after its native build
succeeds. Admission checks registered runner capacity. The worker has a hosted
queue watcher beside the native workload. The watcher allows fifteen minutes for
execution to start, records nonexecution, uploads its result, and cancels only its
own workflow. Checkout, observation, upload, and cancellation have separate step
deadlines. A bootstrap failure also reaches the owned cancellation step.

Once native execution starts, the watcher exits. Qualification requires candidate
validation, observed workload start, and complete workload success. Dispatch
success is not qualification. Capacity admission checks idle eligibility; the
watcher handles a runner disappearing after admission. Missing capacity,
permissions, hosted watcher capacity, or cancellation availability is an explicit
infrastructure failure.

Artifact names include their producer run and attempt. Failed-job reruns locate
the latest producer outcome: a successful producer remains usable only if no
later producer failed. Validation binds the source SHA, repository, main-branch
event, workflow identity, producer outcome, and artifact expiry. Native workers
verify every candidate file before executing it.

Dispatch uses the workflow API's returned run ID. An uncertain response is
correlated with an unguessable title component; time proximity never grants
cancellation authority. Each worker owns its queue deadline independently of the
dispatch response. Cancellation cannot target another platform or workflow.

Protected workers require a fresh workflow run so each workload has an active
queue watcher. A new dispatch may select the same verified candidate. Exact-tag
release qualification requires both native platform workloads; its watcher owns
that release run's deadline and cancellation.

## Verification and diagnostics

Tests exercise success, failure, cancellation, omitted results, invalid inventory,
malformed workflow dependencies, artifact corruption, source changes, and build
reuse. Every fuzz target compiles before scheduled campaigns. Dependency updates
use Bun's native updater and frozen lockfile installation.

Gate wrappers preserve nonzero exit status, elapsed time, source/artifact
identity, and bounded diagnostics. Soaks publish metadata-only observations to
the job log each minute and write atomic local reports. These records exclude
terminal and transcript content. Operational results are retained as workflow
artifacts and task output, outside product documentation.
