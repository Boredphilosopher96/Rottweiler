//! The native ownership fixture is a required sibling of the supplied helper receipt.
use rw_tools::ExecutableArtifactIdentity;
use std::{io, io::Read as _, path::Path};

const HELPER: &str = "rw-sandbox-helper";
const FIXTURE: &str = "rw-sandbox-ownership-fixture";

pub(crate) fn ownership_fixture_identity() -> io::Result<ExecutableArtifactIdentity> {
    let supplied = std::env::var_os("ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT").ok_or_else(|| {
        io::Error::other("run scripts/build-test-helper.py before native fixtures")
    })?;
    load(Path::new(&supplied))
}

fn load(supplied: &Path) -> io::Result<ExecutableArtifactIdentity> {
    let helper = read_identity(supplied)?;
    let directory = supplied
        .parent()
        .ok_or_else(|| io::Error::other("helper receipt has no bundle directory"))?
        .canonicalize()?;
    let fixture = read_identity(&directory.join(format!("{FIXTURE}.identity.json")))?;
    let generation = format!("{}-{}", helper.sha256, fixture.sha256);
    if supplied.file_name() != Some(std::ffi::OsStr::new("rw-sandbox-helper.identity.json"))
        || directory.file_name() != Some(std::ffi::OsStr::new(&generation))
        || helper.executable != directory.join(HELPER)
        || fixture.executable != directory.join(FIXTURE)
    {
        return Err(io::Error::other(
            "native fixture is not bound to the helper bundle",
        ));
    }
    Ok(fixture)
}

fn read_identity(path: &Path) -> io::Result<ExecutableArtifactIdentity> {
    use rustix::fs::{Mode, OFlags};
    let mut file = std::fs::File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(io::Error::other(
            "native fixture receipt must be a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file).take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other(
            "native fixture receipt exceeds 4096 bytes",
        ));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests;
