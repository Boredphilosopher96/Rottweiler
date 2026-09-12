//! The Cargo test binary is an explicitly approved fixture artifact.
#![allow(clippy::expect_used)]
use rw_sandbox::{ExecutableArtifactIdentity, SandboxHelper};
use sha2::{Digest as _, Sha256};
use std::{fs::File, io::Read as _, os::unix::fs::MetadataExt as _, path::Path};

pub fn helper() -> SandboxHelper {
    let executable = Path::new(env!("CARGO_BIN_EXE_rw-sandbox-helper"))
        .canonicalize()
        .expect("Cargo helper path");
    let mut source = File::open(&executable).expect("Cargo helper");
    let metadata = source.metadata().expect("Cargo artifact identity");
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = source.read(&mut buffer).expect("artifact bytes");
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    SandboxHelper::from_artifact(&ExecutableArtifactIdentity {
        executable,
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        sha256: hex_digest(&digest.finalize()),
    })
    .expect("approved Cargo helper snapshot")
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("format digest");
    }
    value
}
