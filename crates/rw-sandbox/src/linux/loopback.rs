//! Fixed kernel route request; no executable lookup after UID namespace remapping.
use super::sandbox_backend;
use crate::SandboxError;
use nix::sys::{
    socket::{self, AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType},
    time::TimeVal,
};
use std::os::fd::AsRawFd as _;

pub(super) fn raise() -> Result<(), SandboxError> {
    let socket = socket::socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkRoute,
    )
    .map_err(sandbox_backend)?;
    socket::setsockopt(
        &socket,
        socket::sockopt::ReceiveTimeout,
        &TimeVal::new(2, 0),
    )
    .map_err(sandbox_backend)?;
    socket::connect(socket.as_raw_fd(), &NetlinkAddr::new(0, 0)).map_err(sandbox_backend)?;
    // nlmsghdr + ifinfomsg + IFLA_IFNAME("lo\0"), aligned to four bytes.
    let mut request = [0_u8; 40];
    request[0..4].copy_from_slice(&40_u32.to_ne_bytes());
    request[4..6].copy_from_slice(&libc::RTM_NEWLINK.to_ne_bytes());
    request[6..8].copy_from_slice(&5_u16.to_ne_bytes()); // REQUEST | ACK
    request[8..12].copy_from_slice(&1_u32.to_ne_bytes());
    request[24..28].copy_from_slice(&1_u32.to_ne_bytes()); // IFF_UP
    request[28..32].copy_from_slice(&1_u32.to_ne_bytes()); // change IFF_UP only
    request[32..34].copy_from_slice(&7_u16.to_ne_bytes());
    request[34..36].copy_from_slice(&3_u16.to_ne_bytes()); // IFLA_IFNAME
    request[36..39].copy_from_slice(b"lo\0");
    let sent =
        socket::send(socket.as_raw_fd(), &request, MsgFlags::empty()).map_err(sandbox_backend)?;
    if sent != request.len() {
        return Err(SandboxError::MalformedHelper);
    }
    let mut reply = [0_u8; 4096];
    let length =
        socket::recv(socket.as_raw_fd(), &mut reply, MsgFlags::empty()).map_err(sandbox_backend)?;
    verify_ack(&reply[..length])
}

fn verify_ack(reply: &[u8]) -> Result<(), SandboxError> {
    if reply.len() < 20 || reply[4..6] != 2_u16.to_ne_bytes() || reply[8..12] != 1_u32.to_ne_bytes()
    {
        return Err(SandboxError::MalformedHelper);
    }
    let declared = u32::from_ne_bytes(reply[..4].try_into().map_err(sandbox_backend)?);
    if usize::try_from(declared).map_err(sandbox_backend)? != reply.len() {
        return Err(SandboxError::MalformedHelper);
    }
    let error = i32::from_ne_bytes(reply[16..20].try_into().map_err(sandbox_backend)?);
    if error != 0 {
        return Err(sandbox_backend(nix::errno::Errno::from_raw(
            error.saturating_neg(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn acknowledgement_requires_complete_matching_success() {
        let mut reply = [0_u8; 20];
        reply[..4].copy_from_slice(&20_u32.to_ne_bytes());
        reply[4..6].copy_from_slice(&2_u16.to_ne_bytes());
        reply[8..12].copy_from_slice(&1_u32.to_ne_bytes());
        assert!(super::verify_ack(&reply).is_ok());
        assert!(super::verify_ack(&reply[..19]).is_err());
        reply[16..20].copy_from_slice(&(-libc::EPERM).to_ne_bytes());
        assert!(super::verify_ack(&reply).is_err());
        reply[16..20].copy_from_slice(&0_i32.to_ne_bytes());
        reply[8..12].copy_from_slice(&2_u32.to_ne_bytes());
        assert!(super::verify_ack(&reply).is_err());
    }
}
