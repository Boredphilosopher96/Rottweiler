//! Declared roots and path projection for the Linux source compiler view.

use crate::SandboxError;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

/// Immutable physical roots for one source-preparation invocation.
/// The caller owns both private directories until process settlement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationFilesystem {
    pub(crate) code: Root,
    pub(crate) work: Root,
    pub(crate) mount: Root,
    pub(crate) output: Option<Root>,
    pub(crate) homes: Vec<PathBuf>,
    pub(crate) credentials: Vec<PathBuf>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    device: u64,
    inode: u64,
}
impl Root {
    fn new(path: &Path) -> Result<Self, SandboxError> {
        let path = fs::canonicalize(path).map_err(invalid)?;
        let metadata = fs::symlink_metadata(&path).map_err(invalid)?;
        if !metadata.is_dir() {
            return Err(SandboxError::RootTypeChanged);
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    pub(crate) fn matches(&self, metadata: &fs::Metadata) -> bool {
        metadata.is_dir() && metadata.dev() == self.device && metadata.ino() == self.inode
    }
}
impl PreparationFilesystem {
    /// Captures disjoint code, private work/view and optional output directory identities.
    ///
    /// # Errors
    /// Returns an error for missing directories or overlapping mutable authority.
    pub fn new(
        code: &Path,
        work: &Path,
        mount: &Path,
        output: Option<&Path>,
    ) -> Result<Self, SandboxError> {
        let homes = crate::linux::linux_homes();
        let credentials = homes
            .iter()
            .flat_map(|home| crate::linux::sensitive_linux_roots(home))
            .collect();
        let layout = Self {
            code: Root::new(code)?,
            work: Root::new(work)?,
            mount: Root::new(mount)?,
            output: output.map(Root::new).transpose()?,
            homes,
            credentials,
        };
        layout.validate()?;
        Ok(layout)
    }
    pub(crate) fn validate(&self) -> Result<(), SandboxError> {
        if self.homes.len() > 8 || self.credentials.len() > 1024 {
            return Err(SandboxError::MalformedHelper);
        }
        let mut roots = vec![&self.code, &self.work, &self.mount];
        roots.extend(self.output.iter());
        for (index, root) in roots.iter().enumerate() {
            if root.path.as_os_str().len() > 4096
                || !root.path.is_absolute()
                || root
                    .path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(SandboxError::MalformedHelper);
            }
            for other in &roots[index + 1..] {
                if (root.device == other.device && root.inode == other.inode)
                    || root.path.starts_with(&other.path)
                    || other.path.starts_with(&root.path)
                {
                    return Err(SandboxError::MalformedHelper);
                }
            }
        }
        if self.homes.iter().chain(&self.credentials).any(|path| {
            path.as_os_str().len() > 4096
                || !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        }) {
            return Err(SandboxError::MalformedHelper);
        }
        Ok(())
    }
    /// Maps physical helper arguments into the declared view.
    ///
    /// # Errors
    /// Rejects absolute paths outside the declared roots.
    pub fn project_argument(&self, argument: &OsStr) -> Result<OsString, SandboxError> {
        let path = Path::new(argument);
        if !path.is_absolute() {
            return Ok(argument.to_owned());
        }
        let roots = [
            Some((&self.code, "/plugin")),
            Some((&self.work, "/scratch")),
            self.output.as_ref().map(|root| (root, "/output")),
        ];
        for (root, projected) in roots.into_iter().flatten() {
            if let Ok(relative) = path.strip_prefix(&root.path) {
                if relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(SandboxError::MalformedHelper);
                }
                return Ok(Path::new(projected).join(relative).into_os_string());
            }
        }
        Err(SandboxError::MalformedHelper)
    }
}
fn invalid(error: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend(error.to_string())
}

#[cfg(test)]
mod tests;

impl crate::SandboxPolicy {
    /// Creates the fixed, network-denied source-preparation filesystem policy.
    ///
    /// # Errors
    /// Rejects invalid or overlapping directory identities.
    pub fn for_preparation(layout: PreparationFilesystem) -> Result<Self, SandboxError> {
        layout.validate()?;
        let mut writable = vec![layout.work.path.clone()];
        writable.extend(layout.output.iter().map(|root| root.path.clone()));
        let mut policy = Self::new(writable, crate::NetworkPolicy::Deny)?;
        policy.preparation = Some(layout);
        Ok(policy)
    }
}
