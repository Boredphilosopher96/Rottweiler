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

#[test]
fn disappearing_discovered_siblings_do_not_abort_declared_grants()
-> Result<(), Box<dyn std::error::Error>> {
    use super::read_grants::{ReadGrant, collect_authorized_read_root, collect_discovered_root};
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let disappearing = root.join("disappearing");
    let declared = root.join("declared");
    std::fs::create_dir(&disappearing)?;
    std::fs::create_dir(&declared)?;
    let entry = std::fs::read_dir(&root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|entry| entry.path() == disappearing)
        .ok_or("missing discovery entry")?;
    std::fs::remove_dir(&disappearing)?;
    let mut grants = std::collections::BTreeMap::new();
    collect_discovered_root(&entry.path(), &root, &[], &mut grants)?;
    assert!(
        grants.is_empty(),
        "missing discovery creates no read authority"
    );
    collect_authorized_read_root(&declared, RootKind::Directory, &[], &mut grants)?;
    collect_discovered_root(&declared, &root, &[], &mut grants)?;
    let grant = grants.get(&declared).ok_or("declared grant missing")?;
    assert!(matches!(grant, ReadGrant::Required(_)));
    assert!(grant.open(&declared)?.is_some());
    std::fs::remove_dir(&declared)?;
    assert!(
        grant.open(&declared).is_err(),
        "declared disappearance is not optional"
    );
    Ok(())
}

#[test]
fn discovered_grant_disappearance_is_optional_but_substitution_is_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    use super::read_grants::collect_discovered_root;
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let sibling = root.join("sibling");
    let outside = root.join("outside");
    std::fs::create_dir(&sibling)?;
    std::fs::create_dir(&outside)?;
    let mut grants = std::collections::BTreeMap::new();
    collect_discovered_root(&sibling, &root, &[], &mut grants)?;
    let grant = grants.get(&sibling).ok_or("discovered grant missing")?;
    std::fs::remove_dir(&sibling)?;
    assert!(
        grant.open(&sibling)?.is_none(),
        "removed sibling grants nothing"
    );
    std::fs::write(&sibling, b"changed kind")?;
    assert!(
        grant.open(&sibling).is_err(),
        "type replacement must fail closed"
    );
    std::fs::remove_file(&sibling)?;
    symlink(&outside, &sibling)?;
    assert!(
        grant.open(&sibling).is_err(),
        "symlink substitution must fail closed"
    );
    Ok(())
}
