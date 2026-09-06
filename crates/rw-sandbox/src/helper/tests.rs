#![allow(clippy::expect_used)]
use crate::{ExecutableArtifactIdentity, SandboxHelper};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::Read as _,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
};

fn fixture() -> (tempfile::TempDir, ExecutableArtifactIdentity) {
    let directory = tempfile::tempdir().expect("artifact directory");
    let executable = directory.path().join("approved-helper");
    let bytes = b"approved internal bootstrap bytes";
    fs::write(&executable, bytes).expect("artifact");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    let executable = executable.canonicalize().expect("canonical artifact");
    let metadata = executable.metadata().expect("identity");
    (
        directory,
        ExecutableArtifactIdentity {
            executable,
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            sha256: crate::executable::hex_digest(&Sha256::digest(bytes)),
        },
    )
}

#[test]
fn approved_bytes_survive_same_inode_mutation_and_path_replacement() {
    let (_directory, approved) = fixture();
    let helper = SandboxHelper::from_artifact(&approved).expect("approve exact bytes");
    fs::write(&approved.executable, b"changed code").expect("mutate installation");
    assert!(SandboxHelper::from_artifact(&approved).is_err());
    fs::remove_file(&approved.executable).expect("remove installation");
    fs::write(&approved.executable, b"replacement").expect("replace installation");
    let mut bytes = Vec::new();
    #[cfg(target_os = "linux")]
    let mut snapshot = fs::File::open(helper.pin().expect("descriptor").0).expect("snapshot");
    #[cfg(not(target_os = "linux"))]
    let mut snapshot = fs::File::open(helper.launch_path()).expect("snapshot");
    snapshot.read_to_end(&mut bytes).expect("snapshot bytes");
    assert_eq!(bytes, b"approved internal bootstrap bytes");
    assert!(SandboxHelper::from_artifact(&approved).is_err());
}

#[test]
fn receipts_reject_unapproved_identity_bytes_and_digest() {
    let (_directory, approved) = fixture();
    for altered in [
        ExecutableArtifactIdentity {
            device: approved.device.wrapping_add(1),
            ..approved.clone()
        },
        ExecutableArtifactIdentity {
            inode: approved.inode.wrapping_add(1),
            ..approved.clone()
        },
        ExecutableArtifactIdentity {
            bytes: approved.bytes + 1,
            ..approved.clone()
        },
        ExecutableArtifactIdentity {
            sha256: "0".repeat(64),
            ..approved.clone()
        },
        ExecutableArtifactIdentity {
            sha256: approved.sha256.to_uppercase(),
            ..approved.clone()
        },
    ] {
        assert!(SandboxHelper::from_artifact(&altered).is_err());
    }
    assert!(SandboxHelper::from_running(&approved.executable).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn approved_snapshot_is_sealed_against_all_later_writes() {
    use std::io::Write as _;
    let (_directory, approved) = fixture();
    let helper = SandboxHelper::from_artifact(&approved).expect("approved snapshot");
    let (path, _pin) = helper.pin().expect("pin");
    let mut writable = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open descriptor");
    assert!(writable.write_all(b"mutate").is_err());
    assert!(writable.set_len(0).is_err());
}
