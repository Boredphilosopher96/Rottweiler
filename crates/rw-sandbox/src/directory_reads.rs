//! Exact ancestor-directory visibility for trusted source preparation on macOS.

use super::{Path, SandboxError, SandboxPolicy};

impl SandboxPolicy {
    /// Permits listing the exact ancestors of one declared code directory.
    ///
    /// Bun's resolver reads these directory entries even for package-local
    /// imports. This does not permit descendant enumeration or file contents.
    /// Ordinary plugin execution must not request this preparation authority.
    /// This API is macOS-only: Landlock directory grants are recursive.
    ///
    /// # Errors
    /// Returns an error for an invalid directory or more than 64 ancestors.
    pub fn with_read_directory_ancestors(mut self, root: &Path) -> Result<Self, SandboxError> {
        let invalid = |source| SandboxError::InvalidReadRoot {
            path: root.to_path_buf(),
            source,
        };
        let root = root.canonicalize().map_err(invalid)?;
        if !root.metadata().map_err(invalid)?.is_dir() {
            return Err(invalid(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory-entry authority requires a directory",
            )));
        }
        let ancestors = root.ancestors().skip(1).collect::<Vec<_>>();
        if ancestors.len() > 64 {
            return Err(invalid(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory-entry authority exceeds its ancestor limit",
            )));
        }
        self.read_directory_ancestors = ancestors.into_iter().map(Path::to_path_buf).collect();
        Ok(self)
    }
}

pub(super) fn append_parameters(policy: &SandboxPolicy, args: &mut Vec<std::ffi::OsString>) {
    for (index, root) in policy.read_directory_ancestors.iter().enumerate() {
        args.push(std::ffi::OsString::from("-D"));
        let mut definition = std::ffi::OsString::from(format!("RW_DIRECTORY_{index}="));
        definition.push(root.as_os_str());
        args.push(definition);
    }
}
