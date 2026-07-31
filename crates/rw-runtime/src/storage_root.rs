use std::{io, path::Path};

/// Creates or validates the current-user private runtime storage root.
///
/// # Errors
/// Returns an error when the path is unsafe or its permissions cannot be secured.
pub fn initialize_private_storage_root(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => return validate_storage_root_type(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage root must have a parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let created = {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(path)
    };
    match created {
        Ok(()) => secure_created_storage_root(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_storage_root_type(&std::fs::symlink_metadata(path)?)
        }
        Err(error) => Err(error),
    }
}

fn validate_storage_root_type(metadata: &std::fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage root must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "existing storage root is not owned by the current user",
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "existing storage root permissions must not grant group or other access",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn secure_created_storage_root(path: &Path) -> io::Result<()> {
    use rustix::fs::{FileType, Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    rustix::fs::fchmod(&descriptor, Mode::from_raw_mode(0o700)).map_err(io::Error::from)?;
    let stat = rustix::fs::fstat(&descriptor).map_err(io::Error::from)?;
    let mode = crate::rustix_mode_bits(Mode::from_raw_mode(stat.st_mode).as_raw_mode()) & 0o777;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || mode != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "new storage root could not be secured for the current user",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_created_storage_root(path: &Path) -> io::Result<()> {
    validate_storage_root_type(&std::fs::symlink_metadata(path)?)
}
