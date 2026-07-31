//! Foreground real-TTY handoff with durable engine gating.

use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read as _, Write as _},
    os::fd::AsFd as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use async_trait::async_trait;
use nix::{
    errno::Errno,
    sys::{
        select::{FdSet, select},
        time::{TimeVal, TimeValLike as _},
    },
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rw_core::ShellId;

const MAX_CAPTURED_TAIL_BYTES: usize = 1024 * 1024;

#[async_trait]
pub trait ShellCompletionGate: Send + Sync {
    /// Persists shell completion after the real child has exited.
    async fn shell_ended(
        &self,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> io::Result<()>;
}

#[async_trait]
pub trait ShellGate: ShellCompletionGate {
    /// Persists shell-active state and resolves only after the durable event
    /// yields the engine-generated shell id.
    async fn shell_started(&self, command: &str) -> io::Result<ShellId>;
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

    /// Returns the active interrupt control byte when the child PTY line
    /// discipline would convert it into `SIGINT`. Raw-mode applications return
    /// `None` so the byte remains ordinary input.
    fn interrupt_input_byte(&self) -> io::Result<Option<u8>> {
        Ok(Some(0x03))
    }
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
    validate_shell_command(command).map_err(TtyError::Parse)?;
    let shell_id = gate
        .shell_started(command)
        .await
        .map_err(TtyError::GateStart)?;
    run_after_durable_shell_start(command, shell_id, gate, spawner, signals, redactor).await
}

/// Runs one foreground child after its engine shell-active event is durable.
/// The trusted CLI broker uses this entry point after observing that event on
/// the authenticated session stream.
pub async fn run_after_durable_shell_start<C, P, S, R>(
    command: &str,
    shell_id: ShellId,
    completion: &C,
    spawner: &P,
    signals: &mut S,
    redactor: &R,
) -> Result<i32, TtyError>
where
    C: ShellCompletionGate + ?Sized,
    P: TerminalSpawner,
    S: TerminalSignalSource,
    R: OutputRedactor + ?Sized,
{
    let argv = local_shell_argv(command).map_err(TtyError::Parse)?;
    run_argv_after_durable_shell_start(&argv, shell_id, completion, spawner, signals, redactor)
        .await
}

fn validate_shell_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        Err("command must not be empty".to_owned())
    } else {
        Ok(())
    }
}

/// Builds the configured user shell invocation used by local `!cmd` escapes.
/// The complete command is one argument to `-lc`, preserving ordinary shell
/// pipelines, redirections, expansions, and interactive program behavior.
pub fn local_shell_argv(command: &str) -> Result<Vec<OsString>, String> {
    validate_shell_command(command)?;
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    Ok(vec![shell, OsString::from("-lc"), OsString::from(command)])
}

/// Runs an already parsed local or remote foreground argv after the durable
/// shell-active event has been observed.
pub async fn run_argv_after_durable_shell_start<C, P, S, R>(
    argv: &[OsString],
    shell_id: ShellId,
    completion: &C,
    spawner: &P,
    signals: &mut S,
    redactor: &R,
) -> Result<i32, TtyError>
where
    C: ShellCompletionGate + ?Sized,
    P: TerminalSpawner,
    S: TerminalSignalSource,
    R: OutputRedactor + ?Sized,
{
    let mut child = match spawner.spawn_tty(argv).await {
        Ok(child) => child,
        Err(error) => {
            let captured = bounded_redacted_tail(redactor, &error.to_string());
            completion
                .shell_ended(shell_id, 127, Some(captured))
                .await
                .map_err(TtyError::GateEnd)?;
            return Err(TtyError::Spawn(error));
        }
    };
    let target = child.signal_target();
    let mut signal_error = None;
    let exit = {
        let wait = child.wait();
        tokio::pin!(wait);
        loop {
            let result = tokio::select! {
                exit = &mut wait => break exit,
                signal = signals.recv() => {
                    signal.and_then(|signal| target.forward(signal))
                }
            };
            if let Err(error) = result {
                signal_error = Some(error);
                break (&mut wait).await;
            }
        }
    };
    let exit = match exit {
        Ok(exit) => exit,
        Err(error) => {
            let captured = bounded_redacted_tail(redactor, &error.to_string());
            completion
                .shell_ended(shell_id, 1, Some(captured))
                .await
                .map_err(TtyError::GateEnd)?;
            return Err(TtyError::Wait(error));
        }
    };
    let captured = exit
        .captured_tail
        .as_deref()
        .map(|tail| bounded_redacted_tail(redactor, tail));
    completion
        .shell_ended(shell_id, exit.status, captured)
        .await
        .map_err(TtyError::GateEnd)?;
    if let Some(error) = signal_error {
        return Err(TtyError::Signal(error));
    }
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

fn bounded_redacted_tail(redactor: &(impl OutputRedactor + ?Sized), value: &str) -> String {
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

#[derive(Debug)]
pub struct TokioTerminalSpawner {
    pump_terminal_input: bool,
    intercept_interrupt_input: bool,
}

impl Default for TokioTerminalSpawner {
    fn default() -> Self {
        Self {
            pump_terminal_input: true,
            intercept_interrupt_input: true,
        }
    }
}

impl TokioTerminalSpawner {
    /// Keeps raw control bytes on the PTY stream so SSH can deliver them to
    /// the remote terminal's foreground process group.
    pub(crate) fn for_remote_tty() -> Self {
        Self {
            pump_terminal_input: true,
            intercept_interrupt_input: false,
        }
    }
}

#[cfg(test)]
impl TokioTerminalSpawner {
    fn without_terminal_input() -> Self {
        Self {
            pump_terminal_input: false,
            intercept_interrupt_input: true,
        }
    }
}

pub struct TokioTerminalChild {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    target: PtySignalTarget,
    cancelled: Arc<AtomicBool>,
    input_thread: Option<thread::JoinHandle<io::Result<()>>>,
    output_thread: Option<thread::JoinHandle<io::Result<()>>>,
    idle_writer: Option<Box<dyn io::Write + Send>>,
    captured_tail: Arc<Mutex<CapturedTail>>,
    terminal_mode: Option<TerminalModeGuard>,
}

impl Drop for TokioTerminalChild {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        drop(self.idle_writer.take());
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        // `TerminalModeGuard` restores the real terminal as this structure is
        // dropped. The bounded polling input thread then observes cancellation
        // without retaining ownership of the user's terminal.
    }
}

#[derive(Clone)]
pub struct PtySignalTarget {
    process_group: rustix::process::Pid,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl SignalTarget for PtySignalTarget {
    fn forward(&self, signal: TerminalSignal) -> io::Result<()> {
        match signal {
            TerminalSignal::Interrupt => {
                let foreground_group = self
                    .master
                    .lock()
                    .map_err(|_| io::Error::other("PTY master lock was poisoned"))?
                    .process_group_leader()
                    .and_then(rustix::process::Pid::from_raw)
                    .unwrap_or(self.process_group);
                forward_interrupt(self.process_group, foreground_group)
            }
            TerminalSignal::WindowChanged => {
                let size = real_terminal_size();
                self.master
                    .lock()
                    .map_err(|_| io::Error::other("PTY master lock was poisoned"))?
                    .resize(size)
                    .map_err(io::Error::other)
            }
        }
    }

    fn interrupt_input_byte(&self) -> io::Result<Option<u8>> {
        let termios = self
            .master
            .lock()
            .map_err(|_| io::Error::other("PTY master lock was poisoned"))?
            .get_termios()
            .ok_or_else(|| io::Error::other("PTY termios is unavailable"))?;
        // portable-pty owns this termios value through its nix version.
        // Compare the stable libc representation so our direct nix major can
        // advance independently without mixing incompatible bitflags types.
        if termios.local_flags.bits() & nix::libc::ISIG == 0 {
            return Ok(None);
        }
        let interrupt = termios.control_chars[nix::libc::VINTR];
        Ok((interrupt != nix::libc::_POSIX_VDISABLE).then_some(interrupt))
    }
}

#[cfg(not(target_os = "linux"))]
fn forward_interrupt(
    _launcher_group: rustix::process::Pid,
    foreground_group: rustix::process::Pid,
) -> io::Result<()> {
    Ok(rustix::process::kill_process_group(
        foreground_group,
        rustix::process::Signal::INT,
    )?)
}

#[cfg(target_os = "linux")]
fn forward_interrupt(
    launcher_group: rustix::process::Pid,
    foreground_group: rustix::process::Pid,
) -> io::Result<()> {
    if foreground_group != launcher_group {
        return Ok(rustix::process::kill_process_group(
            foreground_group,
            rustix::process::Signal::INT,
        )?);
    }
    // `portable_pty` makes the configured user shell the PTY session leader.
    // Non-interactive `dash` keeps its foreground children in that same
    // process group and does not ignore SIGINT while waiting for them. A
    // group-wide signal therefore kills the shell before it can report a
    // child's handled exit status. Keep the shell alive as a job-control
    // monitor and signal every other member, matching an interactive shell's
    // relationship to its foreground job. Open each candidate as a pidfd
    // before rechecking its group and signal only through that stable handle;
    // a concurrent PID exit/reuse can therefore never target another process.
    // Shell builtins have no descendant, so retain the group-wide fallback.
    let mut found_job_member = false;
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(member) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
        else {
            continue;
        };
        if member == launcher_group {
            continue;
        }
        let stat_path = entry.path().join("stat");
        let Ok(stat) = std::fs::read_to_string(&stat_path) else {
            continue;
        };
        if linux_stat_process_group(&stat) != Some(foreground_group.as_raw_pid()) {
            continue;
        }
        let pidfd = match rustix::process::pidfd_open(member, rustix::process::PidfdFlags::empty())
        {
            Ok(pidfd) => pidfd,
            Err(rustix::io::Errno::SRCH) => continue,
            Err(rustix::io::Errno::NOSYS) => {
                return Ok(rustix::process::kill_process_group(
                    foreground_group,
                    rustix::process::Signal::INT,
                )?);
            }
            Err(error) => return Err(error.into()),
        };
        let Ok(stat) = std::fs::read_to_string(stat_path) else {
            continue;
        };
        if linux_stat_process_group(&stat) != Some(foreground_group.as_raw_pid()) {
            continue;
        }
        found_job_member = true;
        match rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::INT) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if found_job_member {
        Ok(())
    } else {
        Ok(rustix::process::kill_process_group(
            foreground_group,
            rustix::process::Signal::INT,
        )?)
    }
}

#[cfg(target_os = "linux")]
fn linux_stat_process_group(stat: &str) -> Option<i32> {
    // `comm` is parenthesized and may itself contain spaces or `)`, so locate
    // its final delimiter before indexing the state, parent, and group fields.
    stat.rfind(") ")?
        .checked_add(2)
        .and_then(|start| stat.get(start..))?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

#[async_trait]
impl TerminalChild for TokioTerminalChild {
    type Target = PtySignalTarget;

    fn signal_target(&self) -> Self::Target {
        self.target.clone()
    }

    async fn wait(&mut self) -> io::Result<TerminalExit> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| io::Error::other("foreground child was already awaited"))?;
        let status = tokio::task::spawn_blocking(move || child.wait())
            .await
            .map_err(|error| io::Error::other(format!("PTY wait task failed: {error}")))
            .and_then(|status| status);
        self.cancelled.store(true, Ordering::Release);
        let input_result = join_io_thread(&mut self.input_thread, "PTY input").await;
        drop(self.idle_writer.take());
        // A login shell can leave helpers from shell startup files holding the
        // slave PTY after the requested foreground command exits. Reap that
        // foreground session before joining the reader; otherwise one inherited
        // descriptor can hang app shutdown forever. Foreground shell mode owns
        // this process group, so detached work must use the background-process
        // tools instead of escaping this lifecycle.
        match rustix::process::kill_process_group(
            self.target.process_group,
            rustix::process::Signal::HUP,
        ) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
        let restore_result = if let Some(mut terminal_mode) = self.terminal_mode.take() {
            terminal_mode.restore()
        } else {
            Ok(())
        };
        // Restore the user's real terminal before waiting for the PTY reader
        // to observe EOF. A descendant that inherited the slave cannot leave
        // the controlling terminal in raw mode after the direct child exits.
        let output_result = join_io_thread(&mut self.output_thread, "PTY output").await;
        let status = status?;
        input_result?;
        output_result?;
        restore_result?;
        let captured_tail = self
            .captured_tail
            .lock()
            .map_err(|_| io::Error::other("captured output lock was poisoned"))?
            .as_string();
        Ok(TerminalExit {
            status: i32::try_from(status.exit_code()).unwrap_or(i32::MAX),
            captured_tail: (!captured_tail.is_empty()).then_some(captured_tail),
        })
    }
}

#[async_trait]
impl TerminalSpawner for TokioTerminalSpawner {
    type Child = TokioTerminalChild;

    async fn spawn_tty(&self, argv: &[OsString]) -> io::Result<Self::Child> {
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        }
        let pair = native_pty_system()
            .openpty(real_terminal_size())
            .map_err(io::Error::other)?;
        let mut command = CommandBuilder::from_argv(argv.to_vec());
        command.set_controlling_tty(true);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        drop(pair.slave);
        let setup = (|| {
            let process_group = pair
                .master
                .process_group_leader()
                .or_else(|| child.process_id().and_then(|pid| i32::try_from(pid).ok()))
                .and_then(rustix::process::Pid::from_raw)
                .ok_or_else(|| io::Error::other("PTY child has no process group"))?;
            let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
            let writer = pair.master.take_writer().map_err(io::Error::other)?;
            let master = Arc::new(Mutex::new(pair.master));
            let captured_tail = Arc::new(Mutex::new(CapturedTail::default()));
            let cancelled = Arc::new(AtomicBool::new(false));
            let terminal_mode = TerminalModeGuard::enter()?;

            let (input_thread, idle_writer) = if self.pump_terminal_input {
                let input_cancelled = Arc::clone(&cancelled);
                let intercept_interrupt_input = self.intercept_interrupt_input;
                let input_target = PtySignalTarget {
                    process_group,
                    master: Arc::clone(&master),
                };
                let input_thread = spawn_terminal_input_thread(
                    writer,
                    input_cancelled,
                    input_target,
                    intercept_interrupt_input,
                )?;
                (Some(input_thread), None)
            } else {
                (None, Some(writer))
            };
            let output_tail = Arc::clone(&captured_tail);
            let output_thread = match thread::Builder::new()
                .name("rw-pty-output".to_owned())
                .spawn(move || pump_terminal_output(&mut reader, &output_tail))
            {
                Ok(thread) => thread,
                Err(error) => {
                    cancelled.store(true, Ordering::Release);
                    if let Some(input_thread) = input_thread {
                        let _ = input_thread.join();
                    }
                    return Err(error);
                }
            };
            Ok((
                process_group,
                master,
                captured_tail,
                cancelled,
                terminal_mode,
                input_thread,
                output_thread,
                idle_writer,
            ))
        })();
        let (
            process_group,
            master,
            captured_tail,
            cancelled,
            terminal_mode,
            input_thread,
            output_thread,
            idle_writer,
        ) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(TokioTerminalChild {
            child: Some(child),
            target: PtySignalTarget {
                process_group,
                master,
            },
            cancelled,
            input_thread,
            output_thread: Some(output_thread),
            idle_writer,
            captured_tail,
            terminal_mode,
        })
    }
}

#[derive(Default)]
struct CapturedTail {
    bytes: VecDeque<u8>,
}

impl CapturedTail {
    fn push(&mut self, bytes: &[u8]) {
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(MAX_CAPTURED_TAIL_BYTES);
        self.bytes.drain(..overflow.min(self.bytes.len()));
        let start = bytes.len().saturating_sub(MAX_CAPTURED_TAIL_BYTES);
        self.bytes.extend(&bytes[start..]);
    }

    fn as_string(&self) -> String {
        let bytes = self.bytes.iter().copied().collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

struct TerminalModeGuard {
    original: Option<rustix::termios::Termios>,
}

impl TerminalModeGuard {
    fn enter() -> io::Result<Option<Self>> {
        let stdin = io::stdin();
        let Ok(original) = rustix::termios::tcgetattr(&stdin) else {
            return Ok(None);
        };
        let mut raw = original.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;
        Ok(Some(Self {
            original: Some(original),
        }))
    }

    fn restore(&mut self) -> io::Result<()> {
        if let Some(original) = self.original.take() {
            rustix::termios::tcsetattr(
                io::stdin(),
                rustix::termios::OptionalActions::Now,
                &original,
            )?;
        }
        Ok(())
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn real_terminal_size() -> PtySize {
    [
        rustix::termios::tcgetwinsize(io::stdout()),
        rustix::termios::tcgetwinsize(io::stdin()),
    ]
    .into_iter()
    .find_map(Result::ok)
    .map_or(
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        |size| PtySize {
            rows: size.ws_row.max(1),
            cols: size.ws_col.max(1),
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        },
    )
}

fn pump_terminal_input(
    writer: &mut (impl io::Write + ?Sized),
    cancelled: &AtomicBool,
    signal_target: &impl SignalTarget,
    intercept_interrupt: bool,
) -> io::Result<()> {
    let mut stdin = io::stdin();
    let mut buffer = [0_u8; 16 * 1024];
    while !cancelled.load(Ordering::Acquire) {
        let mut readable = FdSet::new();
        readable.insert(stdin.as_fd());
        let mut timeout = TimeVal::milliseconds(25);
        match select(None, &mut readable, None, None, &mut timeout) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
        let count = stdin.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if let Err(error) =
            forward_terminal_input(writer, signal_target, &buffer[..count], intercept_interrupt)
        {
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
            ) {
                break;
            }
            return Err(error);
        }
        writer.flush()?;
    }
    Ok(())
}

fn spawn_terminal_input_thread(
    mut writer: Box<dyn io::Write + Send>,
    cancelled: Arc<AtomicBool>,
    signal_target: PtySignalTarget,
    intercept_interrupt: bool,
) -> io::Result<thread::JoinHandle<io::Result<()>>> {
    thread::Builder::new()
        .name("rw-pty-input".to_owned())
        .spawn(move || {
            pump_terminal_input(&mut writer, &cancelled, &signal_target, intercept_interrupt)
        })
}

fn forward_terminal_input(
    writer: &mut (impl io::Write + ?Sized),
    signal_target: &impl SignalTarget,
    bytes: &[u8],
    intercept_interrupt: bool,
) -> io::Result<()> {
    if !intercept_interrupt {
        writer.write_all(bytes)?;
        return writer.flush();
    }
    let Some(interrupt_byte) = signal_target.interrupt_input_byte()? else {
        writer.write_all(bytes)?;
        return writer.flush();
    };
    let mut pending_start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != interrupt_byte {
            continue;
        }
        writer.write_all(&bytes[pending_start..index])?;
        writer.flush()?;
        signal_target.forward(TerminalSignal::Interrupt)?;
        pending_start = index + 1;
    }
    writer.write_all(&bytes[pending_start..])?;
    writer.flush()
}

fn pump_terminal_output(
    reader: &mut (impl io::Read + ?Sized),
    captured_tail: &Mutex<CapturedTail>,
) -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => return Err(error),
        };
        captured_tail
            .lock()
            .map_err(|_| io::Error::other("captured output lock was poisoned"))?
            .push(&buffer[..count]);
        stdout.write_all(&buffer[..count])?;
        stdout.flush()?;
    }
    Ok(())
}

async fn join_io_thread(
    handle: &mut Option<thread::JoinHandle<io::Result<()>>>,
    name: &str,
) -> io::Result<()> {
    let Some(handle) = handle.take() else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || handle.join())
        .await
        .map_err(|error| io::Error::other(format!("{name} join task failed: {error}")))?
        .map_err(|_| io::Error::other(format!("{name} thread panicked")))?
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

/// Builds a dedicated `ssh -t` argv for a remote foreground child. The local
/// side never invokes a shell. The single remote command argument explicitly
/// selects the remote configured shell and quotes the user's complete command
/// as the `-lc` payload.
pub fn remote_tty_argv(host: &str, command: &str) -> Result<Vec<OsString>, String> {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("invalid SSH host".to_owned());
    }
    validate_shell_command(command)?;
    let remote = format!(
        "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
        posix_shell_quote(command)
    );
    Ok(vec![
        OsString::from("ssh"),
        OsString::from("-t"),
        OsString::from("--"),
        OsString::from(host),
        OsString::from(remote),
    ])
}

fn posix_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
    impl ShellCompletionGate for RecordingGate {
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

    #[async_trait]
    impl ShellGate for RecordingGate {
        async fn shell_started(&self, command: &str) -> io::Result<ShellId> {
            self.0
                .lock()
                .expect("events")
                .push(format!("gate-start:{command}"));
            Ok(ShellId("shell-1".to_owned()))
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

    #[derive(Clone)]
    struct ConfiguredInterruptTarget {
        signals: Arc<Mutex<Vec<TerminalSignal>>>,
        interrupt: Option<u8>,
    }

    impl SignalTarget for ConfiguredInterruptTarget {
        fn forward(&self, signal: TerminalSignal) -> io::Result<()> {
            self.signals.lock().expect("signals").push(signal);
            Ok(())
        }

        fn interrupt_input_byte(&self) -> io::Result<Option<u8>> {
            Ok(self.interrupt)
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
                "gate-start:python -q".to_owned(),
                format!(
                    "spawn:{}",
                    std::env::var_os("SHELL")
                        .unwrap_or_else(|| OsString::from("/bin/sh"))
                        .to_string_lossy()
                ),
                "child-exit".to_owned(),
                "gate-end:shell-1:0:token=[REDACTED]".to_owned(),
            ]
        );
        assert_eq!(
            forwarded.lock().expect("signals").as_slice(),
            [TerminalSignal::Interrupt, TerminalSignal::WindowChanged]
        );
    }

    #[test]
    fn raw_terminal_etx_signals_the_job_without_reaching_the_child_line_discipline() {
        let signals = Arc::new(Mutex::new(Vec::new()));
        let target = RecordingTarget(Arc::clone(&signals));
        let mut child_input = Vec::new();
        forward_terminal_input(&mut child_input, &target, b"before\x03after", true)
            .expect("route terminal input");
        assert_eq!(child_input, b"beforeafter");
        assert_eq!(
            signals.lock().expect("signals").as_slice(),
            [TerminalSignal::Interrupt]
        );
    }

    #[test]
    fn remote_terminal_input_preserves_etx_for_the_ssh_pty() {
        let signals = Arc::new(Mutex::new(Vec::new()));
        let target = RecordingTarget(Arc::clone(&signals));
        let mut ssh_input = Vec::new();
        forward_terminal_input(&mut ssh_input, &target, b"before\x03after", false)
            .expect("route remote terminal input");
        assert_eq!(ssh_input, b"before\x03after");
        assert!(signals.lock().expect("signals").is_empty());
    }

    #[test]
    fn child_termios_controls_whether_and_which_input_byte_becomes_sigint() {
        for (interrupt, input, expected_input, expected_signals) in [
            (
                None,
                b"raw\x03input".as_slice(),
                b"raw\x03input".as_slice(),
                0,
            ),
            (
                Some(0x1d),
                b"ctrl-c\x03ctrl-close\x1d".as_slice(),
                b"ctrl-c\x03ctrl-close".as_slice(),
                1,
            ),
        ] {
            let signals = Arc::new(Mutex::new(Vec::new()));
            let target = ConfiguredInterruptTarget {
                signals: Arc::clone(&signals),
                interrupt,
            };
            let mut child_input = Vec::new();
            forward_terminal_input(&mut child_input, &target, input, true)
                .expect("route configured terminal input");
            assert_eq!(child_input, expected_input);
            assert_eq!(signals.lock().expect("signals").len(), expected_signals);
        }
    }

    #[test]
    fn argv_parser_and_remote_tty_do_not_invoke_a_shell() {
        assert_eq!(
            parse_command_argv("python -c 'print(1); import os'").expect("argv"),
            ["python", "-c", "print(1); import os"].map(OsString::from)
        );
        assert_eq!(
            remote_tty_argv("host", "python -q").expect("remote argv"),
            [
                "ssh",
                "-t",
                "--",
                "host",
                "exec \"${SHELL:-/bin/sh}\" -lc 'python -q'",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            remote_tty_argv("host", "printf '%s\\n' \"$HOME\" | sed 's/a/b/'")
                .expect("quoted remote argv")[4],
            OsString::from(
                "exec \"${SHELL:-/bin/sh}\" -lc 'printf '\"'\"'%s\\n'\"'\"' \"$HOME\" | sed '\"'\"'s/a/b/'\"'\"''"
            )
        );
        assert!(remote_tty_argv("-oProxyCommand=bad", "python").is_err());
    }

    #[tokio::test]
    async fn configured_shell_preserves_pipes_redirection_and_expansion() {
        let temporary = tempfile::tempdir().expect("temporary shell workspace");
        let output = temporary.path().join("shell output.txt");
        let command = format!(
            "printf 'pipe' | tr '[:lower:]' '[:upper:]' > {}; printf 'EXPANDED=%s\\n' \"$((2+3))\"",
            posix_shell_quote(&output.to_string_lossy())
        );
        let argv = local_shell_argv(&command).expect("local shell argv");
        assert_eq!(argv.get(1), Some(&OsString::from("-lc")));
        assert_eq!(argv.get(2), Some(&OsString::from(&command)));

        let spawner = TokioTerminalSpawner::without_terminal_input();
        let mut child = spawner.spawn_tty(&argv).await.expect("spawn shell command");
        let exit = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("shell command timeout")
            .expect("shell command wait");
        assert_eq!(exit.status, 0, "shell exit was {exit:?}");
        assert_eq!(
            std::fs::read_to_string(output).expect("redirected output"),
            "PIPE"
        );
        assert!(
            exit.captured_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("EXPANDED=5"))
        );
    }

    #[tokio::test]
    async fn production_spawner_uses_a_real_controlling_pty_and_captures_tee_output() {
        let spawner = TokioTerminalSpawner::without_terminal_input();
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("test -t 0 && test -t 1 && test -t 2 && printf 'PTY_CAPTURE_OK\\n'"),
        ];
        let mut child = spawner.spawn_tty(&argv).await.expect("spawn PTY child");
        let exit = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("PTY child timeout")
            .expect("PTY child wait");
        assert_eq!(exit.status, 0);
        assert!(
            exit.captured_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("PTY_CAPTURE_OK"))
        );
    }

    #[tokio::test]
    async fn production_spawner_forwards_interrupt_to_the_pty_process_group() {
        let spawner = TokioTerminalSpawner::without_terminal_input();
        let argv = [
            OsString::from("/usr/bin/env"),
            OsString::from("python3"),
            OsString::from("-c"),
            OsString::from(
                "import signal,time\n".to_owned()
                    + "def stop(*_):\n print('PTY_INTERRUPT_OK', flush=True); raise SystemExit(23)\n"
                    + "signal.signal(signal.SIGINT, stop)\nprint('PTY_READY', flush=True)\n"
                    + "while True: time.sleep(1)\n",
            ),
        ];
        let mut child = spawner.spawn_tty(&argv).await.expect("spawn PTY child");
        let target = child.signal_target();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if child
                    .captured_tail
                    .lock()
                    .expect("captured output")
                    .as_string()
                    .contains("PTY_READY")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("PTY child readiness timeout");
        target
            .forward(TerminalSignal::Interrupt)
            .expect("forward interrupt");
        let exit = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("interrupted PTY child timeout")
            .expect("interrupted PTY child wait");
        assert_eq!(exit.status, 23, "PTY exit was {exit:?}");
        assert!(
            exit.captured_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("PTY_INTERRUPT_OK"))
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn production_spawner_preserves_a_dash_childs_handled_interrupt_status() {
        let temporary = tempfile::tempdir().expect("temporary shell workspace");
        let script = temporary.path().join("handled-interrupt.py");
        std::fs::write(
            &script,
            concat!(
                "#!/usr/bin/env python3\n",
                "import signal,time\n",
                "def stop(*_):\n",
                "    print('DASH_CHILD_INTERRUPT_OK', flush=True)\n",
                "    raise SystemExit(23)\n",
                "signal.signal(signal.SIGINT, stop)\n",
                "print('DASH_CHILD_READY', flush=True)\n",
                "while True: time.sleep(1)\n",
            ),
        )
        .expect("write child script");
        let mut permissions = std::fs::metadata(&script)
            .expect("child script metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&script, permissions).expect("make child executable");
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-lc"),
            script.as_os_str().to_owned(),
        ];
        let spawner = TokioTerminalSpawner::without_terminal_input();
        let mut child = spawner.spawn_tty(&argv).await.expect("spawn dash child");
        let target = child.signal_target();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if child
                    .captured_tail
                    .lock()
                    .expect("captured output")
                    .as_string()
                    .contains("DASH_CHILD_READY")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dash child readiness timeout");
        target
            .forward(TerminalSignal::Interrupt)
            .expect("forward interrupt");
        let exit = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("interrupted dash child timeout")
            .expect("interrupted dash child wait");
        assert_eq!(exit.status, 23, "PTY exit was {exit:?}");
        assert!(
            exit.captured_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("DASH_CHILD_INTERRUPT_OK"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_stat_parser_handles_spaces_and_closing_parentheses_in_comm() {
        assert_eq!(
            linux_stat_process_group("123 (worker ) name) S 12 345 345 0 -1"),
            Some(345)
        );
        assert_eq!(linux_stat_process_group("malformed"), None);
    }

    #[test]
    fn captured_tail_is_strictly_bounded_to_the_newest_bytes() {
        let mut tail = CapturedTail::default();
        tail.push(&vec![b'a'; MAX_CAPTURED_TAIL_BYTES]);
        tail.push(b"newest");
        assert_eq!(tail.bytes.len(), MAX_CAPTURED_TAIL_BYTES);
        assert!(tail.as_string().ends_with("newest"));
    }
}
