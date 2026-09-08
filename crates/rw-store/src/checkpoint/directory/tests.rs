use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn nested_creation_fences_final_children_before_parent_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let branch = root.path().join("root-0000");
    let leaf = branch.join("checkpoints");
    let mut synced = Vec::new();
    Creation::plan(&leaf)?.publish(|directory| {
        // The first fence sees the complete final tree, not an empty ancestor.
        assert!(leaf.is_dir());
        File::open(directory)?.sync_all()?;
        synced.push(directory.to_path_buf());
        Ok(())
    })?;
    assert_eq!(synced, [leaf.clone(), branch, root.path().to_path_buf()]);
    // Reopening an existing complete namespace does not recreate it.
    fs::write(leaf.join("retained"), b"checkpoint")?;
    create_directory_durable(&leaf)?;
    assert_eq!(fs::read(leaf.join("retained"))?, b"checkpoint");
    Ok(())
}

#[test]
fn existing_regular_file_is_rejected_without_creating_children() -> TestResult {
    let root = tempfile::tempdir()?;
    let occupied = root.path().join("occupied");
    fs::write(&occupied, b"retained")?;
    assert!(create_directory_durable(&occupied).is_err());
    assert!(create_directory_durable(&occupied.join("child")).is_err());
    assert_eq!(fs::read(&occupied)?, b"retained");
    Ok(())
}

#[test]
fn excessive_missing_depth_is_rejected_before_any_directory_creation() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut path = root.path().to_path_buf();
    for _ in 0..65 {
        path.push("child");
    }
    assert!(matches!(
        create_directory_durable(&path),
        Err(CheckpointError::OperationLimit("directory depth"))
    ));
    assert_eq!(fs::read_dir(root.path())?.count(), 0);
    path.pop();
    create_directory_durable(&path)?;
    assert!(path.is_dir());
    Ok(())
}

#[test]
fn concurrently_created_directory_still_receives_its_publication_fence() -> TestResult {
    let root = tempfile::tempdir()?;
    let branch = root.path().join("concurrent");
    let leaf = branch.join("checkpoints");
    let creation = Creation::plan(&leaf)?;
    fs::create_dir(&branch)?;
    let mut synced = Vec::new();
    creation.publish(|directory| {
        File::open(directory)?.sync_all()?;
        synced.push(directory.to_path_buf());
        Ok(())
    })?;
    assert_eq!(synced, [leaf, branch, root.path().to_path_buf()]);
    Ok(())
}

#[test]
fn concurrent_regular_file_aborts_partial_creation_before_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let branch = root.path().join("concurrent");
    let leaf = branch.join("checkpoints");
    let creation = Creation::plan(&leaf)?;
    fs::create_dir(&branch)?;
    fs::write(&leaf, b"other owner")?;
    let mut synced = false;
    assert!(
        creation
            .publish(|_| {
                synced = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!synced);
    assert_eq!(fs::read(leaf)?, b"other owner");
    Ok(())
}

#[test]
fn every_failed_sync_prevents_success_and_stops_further_publication() -> TestResult {
    for failure in 0..3 {
        let root = tempfile::tempdir()?;
        let leaf = root.path().join("root-0000/checkpoints");
        let mut calls = 0;
        let result = Creation::plan(&leaf)?.publish(|directory| {
            let index = calls;
            calls += 1;
            if index == failure {
                return Err(io::Error::other("injected directory sync failure"));
            }
            File::open(directory)?.sync_all()
        });
        assert!(matches!(result, Err(CheckpointError::Io(_))));
        assert_eq!(calls, failure + 1);
        assert!(leaf.is_dir());
        assert_eq!(fs::read_dir(&leaf)?.count(), 0);
    }
    Ok(())
}
