use super::{RootKind, open_landlock_root};
use crate::{NetworkPolicy, SandboxPolicy};
use std::os::fd::AsFd as _;
use std::os::unix::fs::{MetadataExt as _, symlink};

#[test]
fn prepared_read_root_rejects_same_kind_symlink_substitution() {
    let fixture = tempfile::tempdir().expect("root fixture");
    let allowed = fixture.path().join("allowed");
    let outside = fixture.path().join("outside");
    std::fs::create_dir(&allowed).expect("allowed root");
    std::fs::create_dir(&outside).expect("outside root");
    let policy = SandboxPolicy::new([fixture.path()], NetworkPolicy::Deny)
        .expect("write policy")
        .with_read_roots([&allowed])
        .expect("read policy");
    let declared = &policy.read_roots.as_ref().expect("explicit roots")[0];
    let pinned = open_landlock_root(declared, RootKind::Directory).expect("original directory");
    let original = std::fs::metadata(&allowed).expect("original identity");
    std::fs::rename(&allowed, fixture.path().join("retired")).expect("retire original path");
    symlink(&outside, &allowed).expect("substitute same-kind outside directory");
    assert!(
        open_landlock_root(declared, RootKind::Directory).is_err(),
        "same-kind symlink substitution must not extend read authority"
    );
    let retained = rustix::fs::fstat(pinned.as_fd()).expect("retained descriptor");
    assert_eq!(retained.st_ino, original.ino());
    assert_eq!(retained.st_dev, original.dev());
}

#[test]
fn prepared_root_rejects_symlink_substitution_of_an_ancestor() {
    let fixture = tempfile::tempdir().expect("root fixture");
    let parent = fixture.path().join("parent");
    let outside = fixture.path().join("outside");
    std::fs::create_dir_all(parent.join("runtime")).expect("runtime directory");
    std::fs::create_dir_all(outside.join("runtime")).expect("outside runtime directory");
    let policy = SandboxPolicy::new([parent.join("runtime")], NetworkPolicy::Deny)
        .expect("canonical policy");
    std::fs::rename(&parent, fixture.path().join("retired")).expect("retire ancestor");
    symlink(&outside, &parent).expect("substitute ancestor");
    assert!(
        open_landlock_root(&policy.write_roots[0], RootKind::Directory).is_err(),
        "an ancestor symlink must not redirect an approved root"
    );
}
