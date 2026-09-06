use super::{RootKind, open_landlock_root};
use crate::{NetworkPolicy, SandboxPolicy};
use std::os::fd::AsFd as _;
use std::os::unix::fs::{MetadataExt as _, symlink};

#[test]
fn prepared_read_root_rejects_same_kind_symlink_substitution()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let allowed = fixture.path().join("allowed");
    let outside = fixture.path().join("outside");
    std::fs::create_dir(&allowed)?;
    std::fs::create_dir(&outside)?;
    let policy =
        SandboxPolicy::new([fixture.path()], NetworkPolicy::Deny)?.with_read_roots([&allowed])?;
    let declared = &policy.read_roots.as_ref().ok_or("explicit roots missing")?[0];
    let pinned = open_landlock_root(declared, RootKind::Directory)?;
    let original = std::fs::metadata(&allowed)?;
    std::fs::rename(&allowed, fixture.path().join("retired"))?;
    symlink(&outside, &allowed)?;
    assert!(
        open_landlock_root(declared, RootKind::Directory).is_err(),
        "same-kind symlink substitution must not extend read authority"
    );
    let retained = rustix::fs::fstat(pinned.as_fd())?;
    assert_eq!(retained.st_ino, original.ino());
    assert_eq!(retained.st_dev, original.dev());
    Ok(())
}

#[test]
fn prepared_root_rejects_symlink_substitution_of_an_ancestor()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let parent = fixture.path().join("parent");
    let outside = fixture.path().join("outside");
    std::fs::create_dir_all(parent.join("runtime"))?;
    std::fs::create_dir_all(outside.join("runtime"))?;
    let policy = SandboxPolicy::new([parent.join("runtime")], NetworkPolicy::Deny)?;
    std::fs::rename(&parent, fixture.path().join("retired"))?;
    symlink(&outside, &parent)?;
    assert!(
        open_landlock_root(&policy.write_roots[0], RootKind::Directory).is_err(),
        "an ancestor symlink must not redirect an approved root"
    );
    Ok(())
}
