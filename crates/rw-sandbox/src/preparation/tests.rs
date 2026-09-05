#![allow(clippy::expect_used)]
use super::*;
use std::os::fd::AsRawFd as _;

#[test]
fn roots_are_disjoint_and_projection_cannot_escape() {
    let root = tempfile::tempdir().expect("root");
    let create = |name| {
        let path = root.path().join(name);
        fs::create_dir(&path).expect("directory");
        path
    };
    let code = create("code");
    let work = create("work");
    let mount = create("mount");
    let output = create("output");
    let executable = PreparationExecutable::capture(Path::new("/bin/sh")).expect("shell");
    let layout =
        PreparationFilesystem::new(&code, &work, &mount, Some(&output), executable.clone())
            .expect("layout");
    assert_eq!(
        layout
            .project_argument(code.join("entry.ts").as_os_str())
            .expect("entry"),
        OsStr::new("/plugin/entry.ts")
    );
    assert_eq!(
        layout.project_argument(output.as_os_str()).expect("output"),
        OsStr::new("/output/")
    );
    assert!(
        layout
            .project_argument(code.join("../private").as_os_str())
            .is_err()
    );
    assert!(layout.project_argument(OsStr::new("/etc/shadow")).is_err());
    assert!(PreparationFilesystem::new(&code, &work, &code, None, executable.clone()).is_err());
    let child = code.join("writable");
    fs::create_dir(&child).expect("child");
    assert!(PreparationFilesystem::new(&code, &child, &mount, None, executable).is_err());
}

#[test]
fn replaced_root_does_not_match_captured_identity() {
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("code");
    fs::create_dir(&path).expect("code");
    let identity = Root::new(&path).expect("identity");
    fs::rename(&path, root.path().join("previous")).expect("retain old inode");
    fs::create_dir(&path).expect("replace code");
    assert!(!identity.matches(&fs::metadata(path).expect("replacement metadata")));
}

#[test]
fn executable_snapshot_is_immutable_after_original_inode_changes() {
    use std::io::{Read as _, Seek as _, Write as _};
    let root = tempfile::tempdir().expect("fixture");
    let program = root.path().join("program");
    fs::copy("/bin/true", &program).expect("program");
    let expected = fs::read(&program).expect("expected bytes");
    let identity = PreparationExecutable::capture(&program).expect("identity");
    let mut snapshot = identity.snapshot_approved().expect("sealed snapshot");
    let mut original = fs::OpenOptions::new()
        .write(true)
        .open(&program)
        .expect("original file");
    original.write_all(b"!").expect("mutate original inode");
    assert!(
        snapshot.write_all(b"!").is_err(),
        "snapshot refuses existing-descriptor writes"
    );
    assert!(snapshot.set_len(0).is_err(), "snapshot refuses truncation");
    snapshot.rewind().expect("rewind snapshot");
    let mut retained = Vec::new();
    snapshot.read_to_end(&mut retained).expect("snapshot bytes");
    assert_eq!(retained, expected);
    let status = std::process::Command::new(format!("/proc/self/fd/{}", snapshot.as_raw_fd()))
        .status()
        .expect("snapshot execution");
    assert!(
        status.success(),
        "verified snapshot executes after original corruption"
    );
    let seals = rustix::fs::fcntl_get_seals(&snapshot).expect("seals");
    assert!(seals.contains(
        rustix::fs::SealFlags::SEAL
            | rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::GROW
            | rustix::fs::SealFlags::SHRINK
    ));
}
