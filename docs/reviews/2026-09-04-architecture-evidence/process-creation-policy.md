# Worker process-creation policy

Based on b3b021c, SandboxPolicy exposes an explicit without_process_creation policy. macOS denies process-fork. Linux rejects fork/vfork and clone without CLONE_THREAD, and returns ENOSYS for clone3 so libc can fall back to the inspectable clone syscall. Ordinary threads and the initial exec remain permitted.

The Linux proxy relay retains its trusted bootstrap path. The child installs its filesystem, network and process restrictions before executing the target; the relay installs its own filesystem/network floor. The compiler preparation path applies the restriction inside the private preparation view. This leaf provides the policy mechanism; runtime plugin activation must enable it together with host-mediated execution. It does not establish exact filesystem/network authority or deny external service delegation by itself.

Validation on the same source:

- Native macOS ARM64: 20 unit tests and the process driver pass. The driver proves thread creation and process-spawn denial in deny-network and supervised-proxy modes. Strict all-target/all-feature Clippy passes.
- Linux ARM64 in the task-owned namespace-capable Docker diagnostic: 20 unit tests, 16 egress tests, the Linux helper driver, compiler preparation driver and process driver pass. Strict all-target/all-feature Clippy passes.
- The real compiled source-host resolves and bundles the fixture under the restriction; executable replacement and content-change rejection still pass.
- The Linux security gate now explicitly executes both preparation and process-creation drivers.

Raw logs are retained beside this report. Docker ARM64 is not native Linux x64 qualification, and these functional checks are not controlled performance evidence. Fork denial alone does not prove macOS service delegation or arbitrary shell descendant containment.
