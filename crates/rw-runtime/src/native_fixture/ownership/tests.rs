use super::*;
use std::fs;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn bundle(root: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let staging = root.join("staging");
    fs::create_dir(&staging)?;
    fs::write(staging.join(HELPER), b"helper")?;
    fs::write(staging.join(FIXTURE), b"fixture")?;
    let mut helper = ExecutableArtifactIdentity::capture(&staging.join(HELPER), 1024)?;
    let mut fixture = ExecutableArtifactIdentity::capture(&staging.join(FIXTURE), 1024)?;
    let directory = root.join(format!("{}-{}", helper.sha256, fixture.sha256));
    fs::rename(staging, &directory)?;
    helper.executable = directory.join(HELPER);
    fixture.executable = directory.join(FIXTURE);
    let supplied = directory.join(format!("{HELPER}.identity.json"));
    fs::write(&supplied, serde_json::to_vec(&helper)?)?;
    fs::write(
        directory.join(format!("{FIXTURE}.identity.json")),
        serde_json::to_vec(&fixture)?,
    )?;
    Ok(supplied)
}

#[test]
fn exact_sibling_identity_rejects_substituted_executable_before_copy() -> TestResult {
    let root = tempfile::tempdir()?;
    let supplied = bundle(&root.path().canonicalize()?)?;
    let identity = load(&supplied)?;
    let mut copied = tempfile::tempfile()?;
    identity.copy_verified(&mut copied)?;
    fs::write(&identity.executable, b"changed")?;
    assert!(identity.copy_verified(&mut copied).is_err());
    Ok(())
}

#[test]
fn absent_or_changed_sibling_cannot_be_replaced_by_another_bundle() -> TestResult {
    let root = tempfile::tempdir()?;
    let supplied = bundle(&root.path().canonicalize()?)?;
    let identity = load(&supplied)?;
    let sibling = identity.executable.with_extension("identity.json");
    fs::remove_file(&sibling)?;
    assert!(load(&supplied).is_err());
    let mut changed = identity;
    changed.sha256 = "a".repeat(64);
    fs::write(&sibling, serde_json::to_vec(&changed)?)?;
    assert!(load(&supplied).is_err());
    Ok(())
}

#[test]
fn fixture_receipt_rejects_symlink_and_oversized_body() -> TestResult {
    let root = tempfile::tempdir()?;
    let supplied = bundle(&root.path().canonicalize()?)?;
    let identity = load(&supplied)?;
    let sibling = identity.executable.with_extension("identity.json");
    let retained = root.path().join("retained.json");
    fs::rename(&sibling, &retained)?;
    std::os::unix::fs::symlink(&retained, &sibling)?;
    assert!(load(&supplied).is_err());
    fs::remove_file(&sibling)?;
    fs::File::create(&sibling)?.set_len(4097)?;
    assert!(load(&supplied).is_err());
    Ok(())
}
