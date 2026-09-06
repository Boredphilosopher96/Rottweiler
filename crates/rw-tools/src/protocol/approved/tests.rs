#![allow(clippy::expect_used)]
use super::{
    ApprovedProtocolCommand, ExecutableArtifactIdentity, ProtocolChildRequest,
    ProtocolSandboxPolicy,
};
use std::{fs, os::unix::fs::PermissionsExt as _, sync::Arc};

#[test]
fn captured_protocol_bytes_bind_request_and_live_launch_owner() {
    let installation = tempfile::tempdir().expect("installation");
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("workspace path");
    let executable = installation
        .path()
        .canonicalize()
        .expect("installation path")
        .join("runtime");
    fs::write(&executable, b"approved executable fixture").expect("executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    let entry = root.join("entry.js");
    fs::write(&entry, b"approved code").expect("entry");
    let request = ProtocolChildRequest {
        executable: executable.clone(),
        args: vec![entry.to_string_lossy().into_owned()],
        working_directory: Some(root.clone()),
        environment: vec![("MODE".into(), "approved".into())],
        sandbox: ProtocolSandboxPolicy::default(),
    };
    let identity = ExecutableArtifactIdentity::capture(&executable, 4096).expect("identity");
    let code = ExecutableArtifactIdentity::capture(&entry, 4096).expect("code identity");
    let captured = Arc::new(
        ApprovedProtocolCommand::capture(
            &request,
            &identity,
            &[code],
            std::slice::from_ref(&root),
            &root,
        )
        .expect("capture"),
    );
    fs::write(&executable, b"changed executable fixture").expect("replace executable");
    fs::write(&entry, b"changed code").expect("replace code");
    let prepared = captured.prepare(&request).expect("captured request");
    assert_eq!(
        fs::read(&prepared.args[0]).expect("pinned code"),
        b"approved code"
    );
    let code_root = prepared.read_roots[0].clone();
    for changed in [
        ProtocolChildRequest {
            args: vec!["changed".into()],
            ..request.clone()
        },
        ProtocolChildRequest {
            environment: vec![("MODE".into(), "secret-canary".into())],
            ..request.clone()
        },
        ProtocolChildRequest {
            working_directory: None,
            ..request.clone()
        },
        ProtocolChildRequest {
            sandbox: ProtocolSandboxPolicy {
                write_roots: vec![root.clone()],
                ..ProtocolSandboxPolicy::default()
            },
            ..request.clone()
        },
    ] {
        let error = captured
            .prepare(&changed)
            .err()
            .expect("changed request denied");
        assert!(!error.to_string().contains("secret-canary"));
    }
    drop(captured);
    assert!(
        code_root.exists(),
        "physical prepared launch retains captured code"
    );
    drop(prepared);
    assert!(
        !code_root.exists(),
        "last physical owner removes captured code"
    );
}
