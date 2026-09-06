//! Temporary files stay owned until replacement or confirmed rollback.
use crate::ToolError;
#[cfg(unix)]
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct FileTransaction {
    temporary: Option<TemporaryFile>,
}
struct TemporaryFile {
    #[cfg(unix)]
    parent: std::os::fd::OwnedFd,
    #[cfg(unix)]
    name: OsString,
    path: PathBuf,
}
impl FileTransaction {
    #[cfg(unix)]
    pub(super) fn register(
        &mut self,
        parent: std::os::fd::OwnedFd,
        name: OsString,
        path: PathBuf,
    ) -> &std::os::fd::OwnedFd {
        &self
            .temporary
            .insert(TemporaryFile { parent, name, path })
            .parent
    }
    #[cfg(not(unix))]
    pub(super) fn register(&mut self, path: PathBuf) {
        self.temporary = Some(TemporaryFile { path });
    }
    pub(super) fn committed(&mut self) {
        self.temporary = None;
    }
    pub(super) fn cleanup(&mut self) -> Result<(), ToolError> {
        let Some(temporary) = &self.temporary else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            match rustix::fs::unlinkat(
                &temporary.parent,
                &temporary.name,
                rustix::fs::AtFlags::empty(),
            ) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => {
                    return Err(ToolError::EffectsUnsettled(format!(
                        "temporary file {} could not be removed: {error}",
                        temporary.path.display()
                    )));
                }
            }
            rustix::fs::fsync(&temporary.parent).map_err(|error| {
                ToolError::EffectsUnsettled(format!(
                    "temporary file cleanup for {} could not be synchronized: {error}",
                    temporary.path.display()
                ))
            })?;
        }
        #[cfg(not(unix))]
        {
            match std::fs::remove_file(&temporary.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ToolError::EffectsUnsettled(format!(
                        "temporary file {} could not be removed: {error}",
                        temporary.path.display()
                    )));
                }
            }
        }
        self.temporary = None;
        Ok(())
    }
}
