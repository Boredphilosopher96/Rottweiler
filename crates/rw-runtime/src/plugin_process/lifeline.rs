//! Parent-side cancellation authority; helper EOF settles the real effect group.
use super::{PluginProcessError, error};

pub(super) enum ProcessControl {
    Lifeline {
        control: rw_tools::PluginLifeline,
        activation: rw_ext::PluginActivation,
    },
    // Lower-level handoff tests deliberately inject direct child handles.
    #[cfg(test)]
    TestGroup,
}
impl ProcessControl {
    pub(super) fn grant(&mut self) -> Result<(), PluginProcessError> {
        match self {
            Self::Lifeline {
                control,
                activation,
            } => {
                if activation.is_cancelled() {
                    control.stop().map_err(|cause| error(&cause.to_string()))?;
                    return Err(error("plugin activation expired before launch grant"));
                }
                control.grant().map_err(|cause| error(&cause.to_string()))
            }
            #[cfg(test)]
            Self::TestGroup => Ok(()),
        }
    }
    pub(super) fn stop(&self) -> Result<(), PluginProcessError> {
        match self {
            Self::Lifeline { control, .. } => {
                control.stop().map_err(|cause| error(&cause.to_string()))
            }
            #[cfg(test)]
            Self::TestGroup => Ok(()),
        }
    }
    pub(super) fn verify(&mut self) -> Result<(), PluginProcessError> {
        match self {
            Self::Lifeline { control, .. } => control
                .verify_settlement()
                .map_err(|cause| error(&cause.to_string())),
            #[cfg(test)]
            Self::TestGroup => Ok(()),
        }
    }
    pub(super) fn direct(&self) -> bool {
        match self {
            Self::Lifeline { .. } => false,
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
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(cause) => return Err(error(&cause.to_string())),
        }
    }
}

impl super::PluginChild {
    pub(super) fn grant(&self) -> Result<(), PluginProcessError> {
        self.control
            .lock()
            .map_err(|_| error("plugin control lock poisoned"))?
            .as_mut()
            .ok_or_else(|| error("plugin control missing"))?
            .grant()
    }
    pub(super) fn verify_receipt(&self) -> Result<(), PluginProcessError> {
        self.control
            .lock()
            .map_err(|_| error("plugin control lock poisoned"))?
            .as_mut()
            .ok_or_else(|| error("plugin control missing"))?
            .verify()
    }
}
