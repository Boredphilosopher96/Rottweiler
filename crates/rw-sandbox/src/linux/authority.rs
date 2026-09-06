//! Retires namespace setup privileges before any untrusted code or relay thread.
use super::sandbox_backend;
use crate::SandboxError;
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
use std::collections::BTreeMap;

pub(super) fn lock_setup_authority() -> Result<(), SandboxError> {
    use rustix::thread::{CapabilitiesSecureBits as Bits, CapabilitySet, CapabilitySets};
    rustix::thread::clear_ambient_capability_set().map_err(sandbox_backend)?;
    let locked = Bits::NO_ROOT
        | Bits::NO_ROOT_LOCKED
        | Bits::NO_CAP_AMBIENT_RAISE
        | Bits::NO_CAP_AMBIENT_RAISE_LOCKED
        | Bits::KEEP_CAPS_LOCKED;
    if rustix::thread::capabilities_secure_bits().map_err(sandbox_backend)? != locked {
        rustix::thread::set_capabilities_secure_bits(locked).map_err(sandbox_backend)?;
    }
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(sandbox_backend)?;
    let denied = [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_chroot,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_open_by_handle_at,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_mount_setattr,
    ]
    .into_iter()
    .map(|syscall| (syscall, Vec::new()))
    .collect::<BTreeMap<_, _>>();
    let filter: BpfProgram = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(u32::try_from(libc::EPERM).map_err(sandbox_backend)?),
        std::env::consts::ARCH.try_into().map_err(sandbox_backend)?,
    )
    .map_err(sandbox_backend)?
    .try_into()
    .map_err(sandbox_backend)?;
    seccompiler::apply_filter(&filter).map_err(sandbox_backend)
}
