//! Parent-death handling for children owned by the interactive supervisor.

use std::io;

pub const SUPERVISOR_PID_ENV: &str = "ROTTWEILER_SUPERVISOR_PID";

/// Arms the platform parent-death primitive when the supervisor explicitly
/// marked this process as owned. Detached and directly-invoked engines do not
/// receive the marker and remain independent.
pub fn arm_from_environment() -> io::Result<()> {
    let Some(raw_pid) = std::env::var_os(SUPERVISOR_PID_ENV) else {
        return Ok(());
    };
    let expected = parse_supervisor_pid(&raw_pid.to_string_lossy())?;
    arm(expected)
}

fn parse_supervisor_pid(value: &str) -> io::Result<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{SUPERVISOR_PID_ENV} must contain a process id greater than 1"),
            )
        })
}

fn parent_matches(expected: i32) -> bool {
    rustix::process::getppid().is_some_and(|parent| parent.as_raw_nonzero().get() == expected)
}

#[cfg(target_os = "linux")]
fn arm(expected: i32) -> io::Result<()> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    if parent_matches(expected) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Rottweiler supervisor exited during engine startup",
        ))
    }
}

#[cfg(target_os = "macos")]
fn arm(expected: i32) -> io::Result<()> {
    use nix::libc::timespec;
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    let queue = Kqueue::new().map_err(io::Error::from)?;
    let identifier = usize::try_from(expected).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Rottweiler supervisor process id cannot identify a kqueue event",
        )
    })?;
    let change = KEvent::new(
        identifier,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    queue
        .kevent(
            &[change],
            &mut [],
            Some(timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .map_err(io::Error::from)?;
    if !parent_matches(expected) {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Rottweiler supervisor exited during engine startup",
        ));
    }
    std::thread::Builder::new()
        .name("rw-parent-death".to_owned())
        .spawn(move || {
            let mut events = [change];
            if queue.kevent(&[], &mut events, None).is_ok() {
                let _ = rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::TERM,
                );
            }
        })?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn arm(expected: i32) -> io::Result<()> {
    if !parent_matches(expected) {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Rottweiler supervisor exited during engine startup",
        ));
    }
    std::thread::Builder::new()
        .name("rw-parent-death".to_owned())
        .spawn(move || {
            while parent_matches(expected) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::TERM,
            );
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_pid_must_be_a_real_parent_candidate() {
        assert!(matches!(parse_supervisor_pid("42"), Ok(42)));
        for invalid in ["", "0", "1", "-2", "nan"] {
            assert!(
                parse_supervisor_pid(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }
}
