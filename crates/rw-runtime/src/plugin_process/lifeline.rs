//! Parent-side cancellation authority; helper EOF settles the real effect group.
use super::{PluginProcessError, error};
use std::{net::Shutdown, os::unix::net::UnixStream};

pub(super) enum ProcessControl {
    Lifeline(UnixStream),
    // Lower-level handoff tests deliberately inject direct child handles.
    #[cfg(test)]
    TestGroup,
}
impl ProcessControl {
    pub(super) fn grant(&self) -> Result<(), PluginProcessError> {
        use std::io::Write as _;
        match self {
            Self::Lifeline(stream) => {
                let mut target = stream;
                target
                    .write_all(&[1])
                    .map_err(|cause| error(&cause.to_string()))
            }
            #[cfg(test)]
            Self::TestGroup => Ok(()),
        }
    }

    pub(super) fn stop(&self) -> Result<(), PluginProcessError> {
        match self {
            Self::Lifeline(stream) => stream
                .shutdown(Shutdown::Both)
                .map_err(|cause| error(&cause.to_string())),
            #[cfg(test)]
            Self::TestGroup => Ok(()),
        }
    }
    pub(super) fn direct(&self) -> bool {
        match self {
            Self::Lifeline(_) => false,
            #[cfg(test)]
            Self::TestGroup => true,
        }
    }
}

pub(super) async fn group_absent(group: Option<u32>) -> Result<(), PluginProcessError> {
    let Some(pid) = group
        .and_then(|value| i32::try_from(value).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return Ok(());
    };
    loop {
        match rustix::process::test_kill_process_group(pid) {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Ok(()) | Err(rustix::io::Errno::PERM) => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await
            }
            Err(cause) => return Err(error(&cause.to_string())),
        }
    }
}
