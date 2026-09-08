#![allow(clippy::expect_used)]
use super::{ApprovedExecutableImages, ExecutableImageLimits, MAX_ORIGINS};
use crate::ExecutableArtifactIdentity;
use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier, atomic::Ordering},
};

fn artifact(root: &Path, name: &str, bytes: &[u8]) -> ExecutableArtifactIdentity {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(name);
    fs::write(&path, bytes).expect("write source");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("executable");
    ExecutableArtifactIdentity::capture(&path.canonicalize().expect("canonical"), 1024 * 1024)
        .expect("receipt")
}
fn owner(count: usize, bytes: u64) -> ApprovedExecutableImages {
    ApprovedExecutableImages::new(ExecutableImageLimits::new(count, bytes).expect("limits"))
}

#[test]
fn same_bytes_share_backing_without_sharing_installation_authority() {
    let directory = tempfile::tempdir().expect("directory");
    let first = artifact(directory.path(), "first", b"approved executable");
    let second = artifact(directory.path(), "second", b"approved executable");
    let images = owner(1, first.bytes);
    let a = images.acquire(&first).expect("first capture");
    let b = images
        .acquire(&second)
        .expect("same bytes at full capacity");
    assert!(Arc::ptr_eq(&a.0.backing, &b.0.backing));
    assert_eq!(a.installation_path(), first.executable);
    assert_eq!(b.installation_path(), second.executable);
    assert_eq!(images.accounting.images.load(Ordering::Acquire), 1);
    drop((a, b));
    images.close().expect("retired");
}

#[test]
fn cached_image_still_rejects_replaced_inode_and_changed_digest() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = artifact(directory.path(), "source", b"approved");
    let images = owner(2, 1024);
    let retained = images.acquire(&receipt).expect("capture");
    fs::write(&receipt.executable, b"modified").expect("same inode mutation");
    assert!(images.acquire(&receipt).is_err());
    let replacement = artifact(directory.path(), "replacement", b"approved");
    fs::rename(&replacement.executable, &receipt.executable).expect("replace inode");
    assert!(images.acquire(&receipt).is_err());
    assert_eq!(
        fs::read(retained.launch().expect("launch").path()).expect("captured bytes"),
        b"approved"
    );
}

#[test]
fn pressure_preserves_live_image_reuse_without_refunding_its_bytes() {
    let directory = tempfile::tempdir().expect("directory");
    let first = artifact(directory.path(), "first", b"1111");
    let second = artifact(directory.path(), "second", b"2222");
    let images = owner(1, 4);
    let approved = images.acquire(&first).expect("first");
    let launch = approved.launch().expect("launch");
    drop(approved);
    assert!(images.acquire(&second).is_err());
    assert_eq!(images.state.lock().expect("state").entries.len(), 1);
    drop(
        images
            .acquire(&first)
            .expect("pressure preserves same-byte reuse"),
    );
    assert_eq!(images.accounting.bytes.load(Ordering::Acquire), 4);
    assert_eq!(fs::read(launch.path()).expect("still executable"), b"1111");
    drop(launch);
    let replacement = images.acquire(&second).expect("retired capacity reused");
    assert_eq!(
        fs::read(replacement.launch().expect("launch").path()).expect("new bytes"),
        b"2222"
    );
}

#[test]
fn idle_images_reuse_then_evict_under_exact_byte_and_count_limits() {
    let directory = tempfile::tempdir().expect("directory");
    let first = artifact(directory.path(), "first", b"1111");
    let second = artifact(directory.path(), "second", b"22222");
    let images = owner(1, 5);
    let a = images.acquire(&first).expect("first");
    let weak = Arc::downgrade(&a.0.backing);
    drop(a);
    assert!(weak.upgrade().is_some());
    let b = images.acquire(&second).expect("idle eviction");
    assert!(weak.upgrade().is_none());
    assert_eq!(images.accounting.bytes.load(Ordering::Acquire), 5);
    drop(b);
    images.close().expect("close");
    assert_eq!(images.accounting.bytes.load(Ordering::Acquire), 0);
}

#[test]
fn simultaneous_origins_share_one_copy_and_release_each_binding() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = Arc::new(artifact(directory.path(), "source", &[7; 65_536]));
    let images = Arc::new(owner(1, receipt.bytes));
    let barrier = Arc::new(Barrier::new(8));
    let jobs: Vec<_> = (0..8)
        .map(|_| {
            let (images, receipt, barrier) = (images.clone(), receipt.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                images.acquire(&receipt).expect("shared capture")
            })
        })
        .collect();
    let captured: Vec<_> = jobs
        .into_iter()
        .map(|job| job.join().expect("thread"))
        .collect();
    assert!(
        captured
            .iter()
            .all(|image| Arc::ptr_eq(&image.0.backing, &captured[0].0.backing))
    );
    assert_eq!(images.accounting.images.load(Ordering::Acquire), 1);
    assert_eq!(images.accounting.origins.load(Ordering::Acquire), 8);
    drop(captured);
    images.close().expect("all owners retired");
}

#[test]
fn failed_capture_retires_pending_copy_and_allows_valid_retry() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = artifact(directory.path(), "source", b"approved");
    let images = owner(1, receipt.bytes);
    let mut invalid = receipt.clone();
    invalid.sha256 = "0".repeat(64);
    assert!(images.acquire(&invalid).is_err());
    assert_eq!(images.accounting.images.load(Ordering::Acquire), 0);
    assert_eq!(images.accounting.origins.load(Ordering::Acquire), 0);
    assert!(images.state.lock().expect("state").entries.is_empty());
    drop(images.acquire(&receipt).expect("valid retry"));
    images.close().expect("close");
}

#[test]
fn origin_admission_is_bounded_independently_of_shared_content() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = artifact(directory.path(), "source", b"approved");
    let images = owner(1, receipt.bytes);
    let captures: Vec<_> = (0..MAX_ORIGINS)
        .map(|_| images.acquire(&receipt).expect("origin"))
        .collect();
    assert!(images.acquire(&receipt).is_err());
    assert!(images.close().is_err());
    drop(captures);
    images.close().expect("no retained origins");
    assert!(images.acquire(&receipt).is_err());
}

#[test]
fn caller_loss_and_application_drop_preserve_physical_worker_image() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = artifact(directory.path(), "source", b"approved");
    let images = Arc::new(owner(1, receipt.bytes));
    let accounting = Arc::clone(&images.accounting);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_images = Arc::clone(&images);
    let worker = std::thread::spawn(move || {
        let image = worker_images.acquire(&receipt).expect("physical capture");
        let launch = image.launch().expect("launch");
        drop(image);
        drop(worker_images);
        let _ = ready_tx.send(());
        release_rx.recv().expect("release");
        assert_eq!(
            fs::read(launch.path()).expect("retained after caller loss"),
            b"approved"
        );
    });
    ready_rx.recv().expect("physical owner ready");
    drop(ready_rx);
    assert!(images.close().is_err());
    assert!(images.state.lock().expect("state").entries.is_empty());
    drop(images);
    assert_eq!(accounting.bytes.load(Ordering::Acquire), 8);
    release_tx.send(()).expect("release");
    worker.join().expect("physical settlement");
    assert_eq!(accounting.images.load(Ordering::Acquire), 0);
    assert_eq!(accounting.origins.load(Ordering::Acquire), 0);
}

#[test]
fn replacing_approved_source_with_fifo_rejects_without_waiting_for_a_writer() {
    let directory = tempfile::tempdir().expect("directory");
    let receipt = artifact(directory.path(), "source", b"approved");
    let images = owner(1, receipt.bytes);
    drop(images.acquire(&receipt).expect("initial capture"));
    fs::remove_file(&receipt.executable).expect("remove source");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&receipt.executable)
            .status()
            .expect("FIFO fixture")
            .success()
    );
    assert!(images.acquire(&receipt).is_err());
    images.close().expect("failed capture retained no owner");
}

#[derive(Debug)]
pub(in crate::executable) struct RetirementProbe {
    entered: std::sync::mpsc::Sender<()>,
    released: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Drop for RetirementProbe {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self
            .released
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
    }
}

#[test]
fn slow_physical_retirement_holds_credit_but_never_the_publication_lock() {
    for closing in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let first = artifact(directory.path(), "first", b"1111");
        let second = artifact(directory.path(), "second", b"2222");
        let images = Arc::new(owner(1, 4));
        drop(images.acquire(&first).expect("cache image"));
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        {
            let mut state = images.state.lock().expect("state");
            let backing = Arc::get_mut(state.entries[0].backing.as_mut().expect("cached backing"))
                .expect("only cache owns image");
            backing._retirement_probe = Some(RetirementProbe {
                entered,
                released: std::sync::Mutex::new(released),
            });
        }
        let retiring = Arc::clone(&images);
        let replacement = second.clone();
        let retirement = std::thread::spawn(move || {
            if closing {
                retiring.close().expect("close");
            } else {
                drop(retiring.acquire(&replacement).expect("evict then replace"));
            }
        });
        ready.recv().expect("destructor entered");
        assert_eq!(images.accounting.bytes.load(Ordering::Acquire), 4);
        let querying = Arc::clone(&images);
        let (answered, answer) = std::sync::mpsc::channel();
        let query = std::thread::spawn(move || {
            let result = querying.acquire(&second);
            let _ = answered.send(result.is_err());
        });
        let result = answer.recv_timeout(std::time::Duration::from_secs(2));
        release.send(()).expect("release destructor");
        query.join().expect("query");
        retirement.join().expect("retirement");
        assert!(result.expect("concurrent caller was not blocked by filesystem retirement"));
        images.close().expect("all retired");
    }
}
