//! Foreground real-TTY handoff with durable engine gating.

use std::{
    ffi::OsString,
    io,
    os::unix::process::CommandExt as _,
    process::{ExitStatus, Stdio},
};

use async_trait::async_trait;
use rw_core::ShellId;
use tokio::process::{Child, Command};

const MAX_CAPTURED_TAIL_BYTES: usize = 1024 * 1024;

#[async_trait]
pub trait ShellGate: Send + Sync {
    /// Persists shell-active state and resolves only after the durable event
    /// yields the engine-generated shell id.
    async fn shell_started(&self, command: &str) -> io::Result<ShellId>;

    /// Persists shell completion after the real child has exited.
    async fn shell_ended(
        &self,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> io::Result<()>;
}

pub trait OutputRedactor: Send + Sync {
    fn redact(&self, value: &str) -> String;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSignal {
    Interrupt,
    WindowChanged,
}

pub trait SignalTarget: Clone + Send + Sync + 'static {
    fn forward(&self, signal: TerminalSignal) -> io::Result<()>;
}

#[async_trait]
pub trait TerminalChild: Send {
    type Target: SignalTarget;

    fn signal_target(&self) -> Self::Target;
    async fn wait(&mut self) -> io::Result<TerminalExit>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalExit {
    pub status: i32,
    pub captured_tail: Option<String>,
}

#[async_trait]
pub trait TerminalSpawner: Send + Sync {
    type Child: TerminalChild;

    async fn spawn_tty(&self, argv: &[OsString]) -> io::Result<Self::Child>;
}

#[async_trait]
pub trait TerminalSignalSource: Send {
    async fn recv(&mut self) -> io::Result<TerminalSignal>;
}

#[derive(Debug)]
pub enum TtyError {
    Parse(String),
    GateStart(io::Error),
    Spawn(io::Error),
    Wait(io::Error),
    Signal(io::Error),
    GateEnd(io::Error),
}

impl std::fmt::Display for TtyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "invalid foreground command: {message}"),
            Self::GateStart(error) => write!(formatter, "could not enter shell gate: {error}"),
            Self::Spawn(error) => write!(formatter, "could not spawn foreground child: {error}"),
            Self::Wait(error) => write!(formatter, "could not wait for foreground child: {error}"),
            Self::Signal(error) => write!(formatter, "could not forward terminal signal: {error}"),
            Self::GateEnd(error) => write!(formatter, "could not release shell gate: {error}"),
        }
    }
}

impl std::error::Error for TtyError {}

pub async fn run_foreground<G, P, S, R>(
    command: &str,
    gate: &G,
    spawner: &P,
    signals: &mut S,
    redactor: &R,
) -> Result<i32, TtyError>
where
    G: ShellGate,
    P: TerminalSpawner,
    S: TerminalSignalSource,
    R: OutputRedactor,
{
    let argv = parse_command_argv(command).map_err(TtyError::Parse)?;
    let shell_id = gate
        .shell_started(command)
        .await
        .map_err(TtyError::GateStart)?;
    let mut child = match spawner.spawn_tty(&argv).await {
        Ok(child) => child,
        Err(error) => {
            let captured = bounded_redacted_tail(redactor, &error.to_string());
            gate.shell_ended(shell_id, 127, Some(captured))
                .await
                .map_err(TtyError::GateEnd)?;
            return Err(TtyError::Spawn(error));
        }
    };
    let target = child.signal_target();
    let exit = {
        let wait = child.wait();
        tokio::pin!(wait);
        loop {
            tokio::select! {
                exit = &mut wait => break exit.map_err(TtyError::Wait)?,
                signal = signals.recv() => {
                    target.forward(signal.map_err(TtyError::Signal)?)
                        .map_err(TtyError::Signal)?;
                }
            }
        }
    };
    let captured = exit
        .captured_tail
        .as_deref()
        .map(|tail| bounded_redacted_tail(redactor, tail));
    gate.shell_ended(shell_id, exit.status, captured)
        .await
        .map_err(TtyError::GateEnd)?;
    Ok(exit.status)
}

/// Parses an explicit argv command. Quotes and backslashes group arguments;
/// no shell is involved and metacharacters have no execution semantics.
pub fn parse_command_argv(command: &str) -> Result<Vec<OsString>, String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
            started = true;
            continue;
        }
        if character.is_whitespace() && quote.is_none() {
            if started {
                argv.push(OsString::from(std::mem::take(&mut current)));
                started = false;
            }
        } else {
            current.push(character);
            started = true;
        }
    }
    if escaped || quote.is_some() {
        return Err("unterminated quote or escape".to_owned());
    }
    if started {
        argv.push(OsString::from(current));
    }
    if argv.is_empty() || argv[0].is_empty() {
        return Err("command must contain an executable".to_owned());
    }
    Ok(argv)
}

fn bounded_redacted_tail(redactor: &impl OutputRedactor, value: &str) -> String {
    let redacted = redactor.redact(value);
    if redacted.len() <= MAX_CAPTURED_TAIL_BYTES {
        return redacted;
    }
    let start = redacted
        .char_indices()
        .find(|(index, _)| *index >= redacted.len().saturating_sub(MAX_CAPTURED_TAIL_BYTES))
        .map_or(redacted.len(), |(index, _)| index);
    redacted.get(start..).unwrap_or_default().to_owned()
}

#[derive(Debug, Default)]
pub struct TokioTerminalSpawner;

pub struct TokioTerminalChild {
    child: Child,
    target: ProcessGroupTarget,
}

#[derive(Clone, Copy)]
pub struct ProcessGroupTarget(rustix::process::Pid);

impl SignalTarget for ProcessGroupTarget {
    fn forward(&self, signal: TerminalSignal) -> io::Result<()> {
        Ok(rustix::process::kill_process_group(
            self.0,
            match signal {
                TerminalSignal::Interrupt => rustix::process::Signal::INT,
                TerminalSignal::WindowChanged => rustix::process::Signal::WINCH,
            },
        )?)
    }
}

#[async_trait]
impl TerminalChild for TokioTerminalChild {
    type Target = ProcessGroupTarget;

    fn signal_target(&self) -> Self::Target {
        self.target
    }

    async fn wait(&mut self) -> io::Result<TerminalExit> {
        let status = self.child.wait().await?;
        Ok(TerminalExit {
            status: exit_status_code(status),
            captured_tail: None,
        })
    }
}

#[async_trait]
impl TerminalSpawner for TokioTerminalSpawner {
    type Child = TokioTerminalChild;

    async fn spawn_tty(&self, argv: &[OsString]) -> io::Result<Self::Child> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command.as_std_mut().process_group(0);
        let child = command.spawn()?;
        let pid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| io::Error::other("foreground child has no process id"))?;
        Ok(TokioTerminalChild {
            child,
            target: ProcessGroupTarget(pid),
        })
    }
}

fn exit_status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

#[cfg(unix)]
pub struct UnixTerminalSignals {
    interrupt: tokio::signal::unix::Signal,
    window_changed: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl UnixTerminalSignals {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            window_changed: tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::window_change(),
            )?,
        })
    }
}

#[cfg(unix)]
#[async_trait]
impl TerminalSignalSource for UnixTerminalSignals {
    async fn recv(&mut self) -> io::Result<TerminalSignal> {
        tokio::select! {
            value = self.interrupt.recv() => value.map(|()| TerminalSignal::Interrupt),
            value = self.window_changed.recv() => value.map(|()| TerminalSignal::WindowChanged),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "terminal signal stream closed"))
    }
}

/// Builds a dedicated `ssh -t` argv for a remote foreground child. The remote
/// program is still passed as argv after `--`; no local shell is involved.
pub fn remote_tty_argv(host: &str, command: &str) -> Result<Vec<OsString>, String> {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("invalid SSH host".to_owned());
    }
    let remote = parse_command_argv(command)?;
    let mut argv = vec![
        OsString::from("ssh"),
        OsString::from("-t"),
        OsString::from("--"),
        OsString::from(host),
    ];
    argv.extend(remote);
    Ok(argv)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Clone)]
    struct RecordingGate(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl ShellGate for RecordingGate {
        async fn shell_started(&self, command: &str) -> io::Result<ShellId> {
            self.0
                .lock()
                .expect("events")
                .push(format!("gate-start:{command}"));
            Ok(ShellId("shell-1".to_owned()))
        }

        async fn shell_ended(
            &self,
            shell_id: ShellId,
            status: i32,
            captured_output: Option<String>,
        ) -> io::Result<()> {
            self.0.lock().expect("events").push(format!(
                "gate-end:{}:{status}:{}",
                shell_id.0,
                captured_output.unwrap_or_default()
            ));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct RecordingTarget(Arc<Mutex<Vec<TerminalSignal>>>);

    impl SignalTarget for RecordingTarget {
        fn forward(&self, signal: TerminalSignal) -> io::Result<()> {
            self.0.lock().expect("signals").push(signal);
            Ok(())
        }
    }

    struct FixtureChild {
        events: Arc<Mutex<Vec<String>>>,
        target: RecordingTarget,
    }

    #[async_trait]
    impl TerminalChild for FixtureChild {
        type Target = RecordingTarget;

        fn signal_target(&self) -> Self::Target {
            self.target.clone()
        }

        async fn wait(&mut self) -> io::Result<TerminalExit> {
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.events
                .lock()
                .expect("events")
                .push("child-exit".to_owned());
            Ok(TerminalExit {
                status: 0,
                captured_tail: Some("token=SECRET".to_owned()),
            })
        }
    }

    struct FixtureSpawner {
        events: Arc<Mutex<Vec<String>>>,
        signals: Arc<Mutex<Vec<TerminalSignal>>>,
    }

    #[async_trait]
    impl TerminalSpawner for FixtureSpawner {
        type Child = FixtureChild;

        async fn spawn_tty(&self, argv: &[OsString]) -> io::Result<Self::Child> {
            self.events
                .lock()
                .expect("events")
                .push(format!("spawn:{}", argv[0].to_string_lossy()));
            Ok(FixtureChild {
                events: Arc::clone(&self.events),
                target: RecordingTarget(Arc::clone(&self.signals)),
            })
        }
    }

    struct FixtureSignals(VecDeque<TerminalSignal>);

    #[async_trait]
    impl TerminalSignalSource for FixtureSignals {
        async fn recv(&mut self) -> io::Result<TerminalSignal> {
            if let Some(signal) = self.0.pop_front() {
                tokio::task::yield_now().await;
                Ok(signal)
            } else {
                std::future::pending().await
            }
        }
    }

    struct SecretRedactor;

    impl OutputRedactor for SecretRedactor {
        fn redact(&self, value: &str) -> String {
            value.replace("SECRET", "[REDACTED]")
        }
    }

    use std::time::Duration;

    #[tokio::test]
    async fn durable_gate_precedes_spawn_signals_forward_and_end_follows_real_exit() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let gate = RecordingGate(Arc::clone(&events));
        let spawner = FixtureSpawner {
            events: Arc::clone(&events),
            signals: Arc::clone(&forwarded),
        };
        let mut signals = FixtureSignals(VecDeque::from([
            TerminalSignal::Interrupt,
            TerminalSignal::WindowChanged,
        ]));
        assert_eq!(
            run_foreground("python -q", &gate, &spawner, &mut signals, &SecretRedactor)
                .await
                .expect("foreground"),
            0
        );
        assert_eq!(
            events.lock().expect("events").as_slice(),
            [
                "gate-start:python -q",
                "spawn:python",
                "child-exit",
                "gate-end:shell-1:0:token=[REDACTED]",
            ]
        );
        assert_eq!(
            forwarded.lock().expect("signals").as_slice(),
            [TerminalSignal::Interrupt, TerminalSignal::WindowChanged]
        );
    }

    #[test]
    fn argv_parser_and_remote_tty_do_not_invoke_a_shell() {
        assert_eq!(
            parse_command_argv("python -c 'print(1); import os'").expect("argv"),
            ["python", "-c", "print(1); import os"].map(OsString::from)
        );
        assert_eq!(
            remote_tty_argv("host", "python -q").expect("remote argv"),
            ["ssh", "-t", "--", "host", "python", "-q"].map(OsString::from)
        );
        assert!(remote_tty_argv("-oProxyCommand=bad", "python").is_err());
    }
}
