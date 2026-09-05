use std::{env, path::PathBuf};
use thiserror::Error;

mod codegen;
mod signing;

#[derive(Debug, Error)]
enum XtaskError {
    #[error(
        "usage:\n  cargo xtask codegen [--check]\n  cargo xtask sign-update release --root-chain PATH --stable-spec PATH --beta-spec PATH --base-url HTTPS_URL/ --now-unix SECONDS [--previous-stable PATH] [--previous-beta PATH] --artifact PATH --platform PLATFORM [--artifact PATH --platform PLATFORM ...] --release-key KEY_ID=PATH [--release-key KEY_ID=PATH ...] --output DIRECTORY\n  cargo xtask sign-update rotate-root --root-spec PATH [--root-chain PATH] --root-key KEY_ID=PATH [--root-key KEY_ID=PATH ...] --output DIRECTORY\n\nEd25519 private-key files are exact 32-byte seeds and must be owned by the current user, mode 0600, regular, and single-link. Root keys are accepted only by the explicit offline rotate-root command."
    )]
    Usage,
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialize generated protocol artifact: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("generated artifact is stale: {0}")]
    Stale(PathBuf),
    #[error("generated protocol contract is invalid: {0}")]
    GeneratedContract(String),
    #[error("invalid sign-update argument: {0}")]
    SignArgument(String),
    #[error("invalid update metadata spec {path}: {reason}")]
    UpdateSpec { path: PathBuf, reason: String },
    #[error("unsafe Ed25519 private-key file {path}: {reason}")]
    PrivateKey { path: PathBuf, reason: String },
    #[error("could not inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn main() -> Result<(), XtaskError> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("codegen") => codegen::run(arguments),
        Some("sign-update") => signing::run(arguments),
        _ => Err(XtaskError::Usage),
    }
}
