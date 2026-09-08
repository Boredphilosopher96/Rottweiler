use super::*;
use std::fs;

fn paths(walk: impl Iterator<Item = Result<Entry, ToolError>>, root: &Path) -> Vec<PathBuf> {
    walk.map(|entry| {
        entry
            .expect("bounded entry")
            .path
            .strip_prefix(root)
            .unwrap()
            .to_owned()
    })
    .collect()
}

#[test]
fn nested_ignore_pruning_hidden_and_symlink_order_match_sorted_walker() {
    let root = tempfile::tempdir().unwrap();
    for path in [".git", "a/nested", "b/pruned", "c/.git", ".hidden"] {
        fs::create_dir_all(root.path().join(path)).unwrap();
    }
    for path in [
        "z.txt",
        "a/z.txt",
        "a/a.txt",
        "a/nested/keep.txt",
        "a/nested/drop.txt",
        "b/pruned/no.txt",
        "b/keep.txt",
        "c/included.txt",
        ".hidden/no.txt",
    ] {
        fs::write(root.path().join(path), "text").unwrap();
    }
    fs::write(root.path().join(".gitignore"), "b/pruned/\n*.tmp\n").unwrap();
    fs::write(root.path().join("a/.ignore"), "nested/drop.txt\n").unwrap();
    fs::write(
        root.path().join("a/.gitignore"),
        "*.txt\n!a.txt\n!nested/\n!nested/keep.txt\n",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("a"), root.path().join("linked")).unwrap();
    let expected = WalkBuilder::new(root.path())
        .standard_filters(true)
        .follow_links(false)
        .sort_by_file_path(Path::cmp)
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.path().strip_prefix(root.path()).unwrap().to_owned())
        .collect::<Vec<_>>();
    let actual = paths(
        BoundedWalk::new(root.path(), true, &CancellationToken::default()).unwrap(),
        root.path(),
    );
    assert_eq!(actual, expected);
    assert!(
        !actual
            .iter()
            .any(|path| path.ends_with("no.txt") || path.ends_with("drop.txt"))
    );
    let shallow = paths(
        BoundedWalk::new(root.path(), false, &CancellationToken::default()).unwrap(),
        root.path(),
    );
    let expected_shallow = WalkBuilder::new(root.path())
        .max_depth(Some(1))
        .standard_filters(true)
        .follow_links(false)
        .sort_by_file_path(Path::cmp)
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.path().strip_prefix(root.path()).unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(shallow, expected_shallow);
}

#[test]
fn sorting_rejects_before_exposing_an_arbitrary_partial_directory() {
    let root = tempfile::tempdir().unwrap();
    for file in ["c", "a", "b"] {
        fs::write(root.path().join(file), "").unwrap();
    }
    let mut walk = BoundedWalk::with_limits(
        root.path(),
        true,
        &CancellationToken::default(),
        2,
        MAX_PENDING_PATH_BYTES,
    )
    .unwrap();
    assert_eq!(walk.next().unwrap().unwrap().path(), root.path());
    assert!(walk.next().unwrap().is_err());
    assert_eq!(walk.children.len(), 2);
    assert!(walk.next().is_none());
}

#[test]
fn queued_sibling_and_descendant_paths_share_one_allowance() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("a")).unwrap();
    fs::write(root.path().join("z"), "").unwrap();
    for file in ["a/1", "a/2"] {
        fs::write(root.path().join(file), "").unwrap();
    }
    let mut walk = BoundedWalk::with_limits(
        root.path(),
        true,
        &CancellationToken::default(),
        2,
        MAX_PENDING_PATH_BYTES,
    )
    .unwrap();
    assert!(walk.next().unwrap().is_ok());
    assert_eq!(walk.next().unwrap().unwrap().path(), root.path().join("a"));
    assert!(
        walk.next().unwrap().is_err(),
        "the queued z sibling still consumes a slot"
    );
}

#[test]
fn cancellation_stops_before_another_directory_read() {
    let root = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::default();
    let mut walk = BoundedWalk::new(root.path(), true, &cancel).unwrap();
    assert!(walk.next().unwrap().is_ok());
    cancel.cancel();
    assert!(matches!(walk.next(), Some(Err(ToolError::Cancelled))));
    assert!(walk.next().is_none());
}
