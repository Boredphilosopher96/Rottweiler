#![allow(clippy::expect_used)]
use super::{CloneFlags, File, Mode, NAME, OFlags, Permissions, create, create_with};
use crate::{ApprovedExecutable, ExecutableArtifactIdentity};
use std::{
    fs,
    io::{Seek as _, SeekFrom, Write as _},
    os::unix::{
        ffi::OsStrExt as _,
        fs::{MetadataExt as _, PermissionsExt as _},
    },
};

fn fixture() -> (tempfile::TempDir, ExecutableArtifactIdentity, File) {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("source");
    fs::write(&path, b"approved executable bytes").expect("source bytes");
    fs::set_permissions(&path, Permissions::from_mode(0o700)).expect("executable");
    let path = path.canonicalize().expect("source path");
    let approved = ExecutableArtifactIdentity::capture(&path, 1024).expect("receipt");
    let source = File::open(&path).expect("pinned source");
    (root, approved, source)
}

fn clone(source: &File, parent: &File) -> rustix::io::Result<()> {
    rustix::fs::fclonefileat(
        source,
        parent,
        NAME,
        CloneFlags::NOFOLLOW | CloneFlags::NOOWNERCOPY,
    )
}

#[test]
fn actual_clone_is_private_and_independent_of_later_source_writes() {
    let (_root, approved, source) = fixture();
    (&source)
        .seek(SeekFrom::End(0))
        .expect("source hash already consumed descriptor");
    let (directory, path, file) = create_with(&approved, &source, clone).expect("actual COW clone");
    assert_eq!(
        directory.path().metadata().expect("directory").mode() & 0o777,
        0o700
    );
    assert_ne!(
        file.metadata().expect("clone").ino(),
        source.metadata().expect("source").ino()
    );
    fs::write(&approved.executable, b"replacement mutable bytes").expect("mutate source");
    assert_eq!(
        fs::read(&path).expect("retained clone"),
        b"approved executable bytes"
    );
    drop(file);
    let parent = directory.path().to_path_buf();
    drop(directory);
    assert!(!path.exists());
    assert!(!parent.exists());
}

#[test]
fn read_only_executable_source_still_publishes_verified_snapshot() {
    let (_root, approved, _source) = fixture();
    fs::set_permissions(&approved.executable, Permissions::from_mode(0o500))
        .expect("read-only source");
    let image = ApprovedExecutable::from_artifact(&approved).expect("read-only snapshot");
    let path = image.launch_path().to_path_buf();
    assert_eq!(
        fs::read(&path).expect("snapshot"),
        b"approved executable bytes"
    );
    assert_eq!(path.metadata().expect("mode").mode() & 0o777, 0o500);
    drop(image);
    assert!(!path.exists());
}

#[test]
fn capture_uses_pinned_descriptor_after_installation_path_replacement() {
    let (root, approved, source) = fixture();
    fs::rename(&approved.executable, root.path().join("original")).expect("retain original");
    fs::write(&approved.executable, b"replacement mutable bytes").expect("substituted pathname");
    let (_directory, path, _file) = create(&approved, &source).expect("exact pinned original");
    assert_eq!(
        fs::read(path).expect("snapshot"),
        b"approved executable bytes"
    );
    assert!(
        ApprovedExecutable::from_artifact(&approved).is_err(),
        "a new acquisition rejects replacement"
    );
}

#[test]
fn changed_source_after_clone_is_rejected_and_directory_removed() {
    let (_root, approved, source) = fixture();
    let mut retained_parent = None;
    let outcome = create_with(&approved, &source, |source, parent| {
        let path = rustix::fs::getpath(parent).expect("exact private directory path");
        retained_parent = Some((
            parent.try_clone().expect("observe directory retirement"),
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())),
        ));
        clone(source, parent)?;
        fs::write(&approved.executable, b"replacement mutable bytes").expect("change after clone");
        fs::set_permissions(&approved.executable, Permissions::from_mode(0o500))
            .expect("mode change");
        Ok(())
    });
    assert!(outcome.is_err());
    let (parent, path) = retained_parent.expect("directory was created");
    assert_eq!(
        fs::symlink_metadata(&path)
            .expect_err("private directory removed")
            .kind(),
        std::io::ErrorKind::NotFound,
    );
    assert_eq!(
        rustix::fs::openat(
            &parent,
            NAME,
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty()
        )
        .expect_err("pinned directory contains no failed snapshot"),
        rustix::io::Errno::NOENT,
    );
}

#[test]
fn destination_digest_is_checked_even_when_original_bytes_are_approved() {
    let (_root, approved, source) = fixture();
    let outcome = create_with(&approved, &source, |_, parent| {
        let mut file = File::from(rustix::fs::openat(
            parent,
            NAME,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        file.write_all(b"replacement mutable bytes")
            .expect("bad snapshot");
        Ok(())
    });
    assert!(outcome.is_err());
    assert_eq!(
        fs::read(&approved.executable).expect("unchanged original"),
        b"approved executable bytes"
    );
}

#[test]
fn unsupported_clone_uses_same_verified_copy_but_other_errors_fail() {
    for error in [
        rustix::io::Errno::XDEV,
        rustix::io::Errno::NOTSUP,
        rustix::io::Errno::NOSYS,
    ] {
        let (_root, approved, source) = fixture();
        let (_directory, path, _file) =
            create_with(&approved, &source, |_, _| Err(error)).expect("verified copy");
        assert_eq!(fs::read(path).expect("copy"), b"approved executable bytes");
    }
    for error in [
        rustix::io::Errno::PERM,
        rustix::io::Errno::IO,
        rustix::io::Errno::NOSPC,
    ] {
        let (_root, approved, source) = fixture();
        assert!(create_with(&approved, &source, |_, _| Err(error)).is_err());
    }
}

#[test]
fn failed_clone_with_partial_destination_never_overwrites_it() {
    let (_root, approved, source) = fixture();
    assert!(
        create_with(&approved, &source, |_, parent| {
            let _file = rustix::fs::openat(
                parent,
                NAME,
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
                Mode::RUSR | Mode::WUSR,
            )?;
            Err(rustix::io::Errno::NOTSUP)
        })
        .is_err()
    );
}
