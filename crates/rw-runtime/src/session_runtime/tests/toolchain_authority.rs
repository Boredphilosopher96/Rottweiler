use crate::session_runtime::command_execution::{CommandFixtureMode, build_command_executor};
use crate::session_runtime::toolchain::{HookCommandCapture, ToolchainRuntime};
use crate::session_runtime::toolchain_authority::build_toolchain_executor;
use rw_tools::{
    BashSandboxMode, CancellationToken, CommandExecutor, CommandRequest, CommandSafetyClassifier,
    ExecutionLease,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

async fn run(executor: &Arc<dyn CommandExecutor>, cwd: &Path, command: String) -> i32 {
    let result = executor
        .run(
            CommandRequest {
                command,
                cwd: cwd.to_path_buf(),
                env: BTreeMap::new(),
                network_domains: Vec::new(),
                sandbox: BashSandboxMode::Sandboxed,
            },
            CancellationToken::default(),
            Arc::new(HookCommandCapture::default()),
        )
        .await
        .expect("sandbox command must settle");
    executor
        .settle_effects()
        .await
        .expect("physical command proof");
    result.exit_code
}

#[tokio::test]
async fn runtime_reads_are_toolchain_only_read_only_and_generation_scoped() {
    let workspace = tempfile::tempdir().expect("workspace");
    // /tmp and /usr belong to the reviewed shell baseline. /dev/shm does not,
    // so a successful positive probe here requires our explicit read grant.
    let external = tempfile::tempdir_in("/dev/shm").expect("external runtime fixture");
    let first = external.path().join("first");
    let second = external.path().join("second");
    std::fs::create_dir(&first).expect("first runtime");
    std::fs::create_dir(&second).expect("second runtime");
    let first_file = first.join("data");
    let second_file = second.join("data");
    std::fs::write(&first_file, "first").expect("runtime data");
    std::fs::write(&second_file, "second").expect("outside data");
    let alias = external.path().join("selected");
    std::os::unix::fs::symlink(&first, &alias).expect("canonical read root alias");
    let lease = Arc::new(ExecutionLease::acquire(workspace.path().join("lease")).expect("lease"));
    let safety = Arc::new(CommandSafetyClassifier::default());
    let roots = vec![workspace.path().to_path_buf()];
    let ordinary = build_command_executor(
        &roots,
        workspace.path(),
        CommandFixtureMode::Live,
        &lease,
        &safety,
        None,
    )
    .expect("ordinary executor");
    let toolchain = build_toolchain_executor(
        &roots,
        &[alias],
        workspace.path(),
        CommandFixtureMode::Live,
        &lease,
        &safety,
    )
    .expect("toolchain executor");
    assert_scoped_reads(
        &ordinary,
        &toolchain,
        workspace.path(),
        &first_file,
        &second_file,
    )
    .await;
    let runtime = ToolchainRuntime::new_with_read_only(
        ordinary.clone(),
        toolchain,
        ordinary.clone(),
        workspace.path().to_path_buf(),
        &roots,
    );
    let replacement = build_toolchain_executor(
        &roots,
        &[second],
        workspace.path(),
        CommandFixtureMode::Live,
        &lease,
        &safety,
    )
    .expect("replacement toolchain");
    runtime.prepare(
        1,
        ordinary.clone(),
        replacement,
        ordinary,
        workspace.path().to_path_buf(),
        &roots,
    );
    runtime.commit(1);
    let current = runtime.current();
    assert_eq!(
        run(
            &current.toolchain_executor,
            workspace.path(),
            read(&second_file)
        )
        .await,
        0
    );
    assert_ne!(
        run(
            &current.toolchain_executor,
            workspace.path(),
            read(&first_file)
        )
        .await,
        0,
        "replacement must not retain retired runtime read grants"
    );
}

fn read(path: &Path) -> String {
    format!("cat {}", shell_words::quote(path.to_str().expect("UTF-8")))
}

async fn assert_scoped_reads(
    ordinary: &Arc<dyn CommandExecutor>,
    toolchain: &Arc<dyn CommandExecutor>,
    workspace: &Path,
    first: &Path,
    second: &Path,
) {
    assert_eq!(run(toolchain, workspace, read(first)).await, 0);
    assert_ne!(
        run(ordinary, workspace, read(first)).await,
        0,
        "ordinary Bash must not inherit toolchain read authority"
    );
    assert_ne!(
        run(toolchain, workspace, read(second)).await,
        0,
        "a configured runtime root must not authorize its sibling"
    );
    let write = format!(
        "printf changed > {}",
        shell_words::quote(first.to_str().expect("UTF-8"))
    );
    assert_ne!(run(toolchain, workspace, write).await, 0);
    assert_eq!(
        std::fs::read_to_string(first).expect("unchanged runtime"),
        "first"
    );
}
