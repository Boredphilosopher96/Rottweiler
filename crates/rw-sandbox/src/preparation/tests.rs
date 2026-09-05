#![allow(clippy::expect_used)]
use super::*;

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
    let layout = PreparationFilesystem::new(&code, &work, &mount, Some(&output)).expect("layout");
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
    assert!(PreparationFilesystem::new(&code, &work, &code, None).is_err());
    let child = code.join("writable");
    fs::create_dir(&child).expect("child");
    assert!(PreparationFilesystem::new(&code, &child, &mount, None).is_err());
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
