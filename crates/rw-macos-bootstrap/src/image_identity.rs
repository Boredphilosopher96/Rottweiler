//! Kernel vnode identity for the executable mapping containing this static library.
use std::{io, mem};

const PROC_PIDREGIONPATHINFO: i32 = 8;
const VM_PROT_EXECUTE: u32 = 4;

// sys/proc_info.h: proc_regioninfo followed by vnode_info_path. Accounting
// counters are not interpreted; their fourteen uint32_t slots preserve the ABI.
#[repr(C)]
struct RegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    accounting: [u32; 14],
    address: u64,
    size: u64,
}
#[repr(C)]
struct RegionPath {
    region: RegionInfo,
    vnode: libc::vnode_info_path,
}

pub(super) fn running() -> io::Result<(u64, u64)> {
    let address = running as *const () as usize as u64;
    // SAFETY: These C records contain only integer scalars and fixed arrays;
    // zero is valid for every field and initializes padding before the call.
    let mut info: RegionPath = unsafe { mem::zeroed() };
    let size = i32::try_from(mem::size_of::<RegionPath>())
        .map_err(|_| io::Error::other("running image record is too large"))?;
    let pid = i32::try_from(std::process::id())
        .map_err(|_| io::Error::other("running process identifier is invalid"))?;
    // SAFETY: The output pointer names an initialized, writable RegionPath of
    // exactly `size` bytes. The kernel inspects our process at a code address;
    // it neither retains the pointer nor changes Rust-owned pointer fields.
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDREGIONPATHINFO,
            address,
            (&raw mut info).cast(),
            size,
        )
    };
    if count != size {
        return Err(io::Error::other("running image vnode proof is unavailable"));
    }
    if info.region.protection & VM_PROT_EXECUTE == 0
        || address < info.region.address
        || address >= info.region.address.saturating_add(info.region.size)
        || info.vnode.vip_vi.vi_stat.vst_ino == 0
    {
        return Err(io::Error::other("running image vnode proof is invalid"));
    }
    Ok((
        u64::from(info.vnode.vip_vi.vi_stat.vst_dev),
        info.vnode.vip_vi.vi_stat.vst_ino,
    ))
}

#[cfg(test)]
mod tests {
    use super::{RegionInfo, RegionPath};
    #[test]
    fn records_match_the_macos_proc_info_abi() {
        assert_eq!(std::mem::size_of::<RegionInfo>(), 96);
        assert_eq!(std::mem::offset_of!(RegionPath, vnode), 96);
        assert_eq!(std::mem::size_of::<RegionPath>(), 1272);
        assert!(super::running().is_ok());
    }
}
