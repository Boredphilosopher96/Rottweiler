#![allow(clippy::expect_used)]
use super::super::{InputLine, input};
use super::{
    Terminal,
    io::duplicate,
    lines::{Lines, MAX_ECHO_BYTES, MAX_LINE_BYTES},
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Semaphore, watch};

async fn fixture() -> (
    input::InputReceiver,
    super::Interrupts,
    Terminal,
    UnixStream,
    UnixStream,
) {
    let (input, input_peer) = UnixStream::pair().expect("input descriptors");
    let (output, output_peer) = UnixStream::pair().expect("output descriptors");
    let active = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("terminal admission");
    let (receive, interrupts, terminal) = Terminal::spawn(
        duplicate(&input).expect("input"),
        duplicate(&output).expect("output"),
        active,
    )
    .await
    .expect("worker");
    (receive, interrupts, terminal, input_peer, output_peer)
}

#[tokio::test]
async fn bounded_lines_preserve_utf8_crlf_and_refuse_oversize_without_truncation() {
    let (send, mut receive) = input::channel();
    let (interrupts, _) = watch::channel(());
    let mut lines = Lines::new(false);
    let mut echo = Vec::with_capacity(MAX_ECHO_BYTES);
    let exact = "é".repeat(MAX_LINE_BYTES / 2);
    lines
        .push(exact.as_bytes(), &send, &mut echo, &interrupts)
        .expect("exact limit");
    lines
        .push(b"\r", &send, &mut echo, &interrupts)
        .expect("submit");
    lines
        .push(b"\nnext", &send, &mut echo, &interrupts)
        .expect("split CRLF");
    lines.eof(&send).expect("final partial line");
    assert!(
        matches!(receive.recv().await.expect("exact text").value, InputLine::Line(text) if text == exact)
    );
    assert!(
        matches!(receive.recv().await.expect("partial text").value, InputLine::Line(text) if text == "next")
    );
    assert!(matches!(
        receive.recv().await.expect("EOF").value,
        InputLine::Eof
    ));
    assert!(echo.is_empty());
    let mut oversized = Lines::new(false);
    oversized
        .push(exact.as_bytes(), &send, &mut echo, &interrupts)
        .expect("limit");
    assert!(
        oversized
            .push(b"x", &send, &mut echo, &interrupts)
            .expect_err("no truncation")
            .to_string()
            .contains("128 KiB")
    );
    let mut invalid = Lines::new(false);
    assert!(
        invalid
            .push(&[0xff, b'\n'], &send, &mut echo, &interrupts)
            .is_err()
    );
}

#[tokio::test]
async fn interactive_interrupt_is_coalesced_independently_of_full_input_queue() {
    let (send, mut receive) = input::channel();
    for _ in 0..rw_types::MAX_CLIENT_CONTROLS {
        send.admit(InputLine::Line("pending".to_owned()))
            .expect("slot")
            .publish();
    }
    let (interrupts, mut observed) = watch::channel(());
    let mut lines = Lines::new(true);
    let mut echo = Vec::with_capacity(MAX_ECHO_BYTES);
    lines
        .push(b"draft\x03\x03", &send, &mut echo, &interrupts)
        .expect("urgent interrupt bypasses data queue");
    observed.changed().await.expect("interrupt");
    assert!(send.failure.message().is_none());
    assert!(
        matches!(receive.recv().await.expect("unchanged first line").value, InputLine::Line(text) if text == "pending")
    );
}

#[tokio::test]
async fn terminal_shutdown_wakes_blocked_output_and_retains_request_until_settlement() {
    let (_input, _interrupts, mut terminal, _input_peer, _output_peer) = fixture().await;
    {
        let writing = terminal.print("output".repeat(1024 * 1024));
        tokio::pin!(writing);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut writing)
                .await
                .is_err()
        );
    }
    assert_eq!(terminal.output_slot.available_permits(), 0);
    tokio::time::timeout(Duration::from_secs(2), terminal.close())
        .await
        .expect("wake bypasses congested output")
        .expect("physical close");
    assert_eq!(terminal.output_slot.available_permits(), 1);
}

#[tokio::test]
async fn terminal_pipe_lines_output_order_and_physical_close() {
    let (mut input, _interrupts, mut terminal, mut input_peer, mut output_peer) = fixture().await;
    input_peer.write_all("héllo\n".as_bytes()).expect("input");
    let received = tokio::time::timeout(Duration::from_secs(5), input.recv())
        .await
        .expect("input deadline")
        .expect("input delivery");
    assert!(matches!(received.value, InputLine::Line(text) if text == "héllo"));
    terminal.print("first".to_owned()).await.expect("first");
    terminal.print("second".to_owned()).await.expect("second");
    let mut bytes = [0_u8; 11];
    output_peer.read_exact(&mut bytes).expect("ordered output");
    assert_eq!(&bytes, b"firstsecond");
    tokio::time::timeout(Duration::from_secs(5), terminal.close())
        .await
        .expect("close wakes idle poll")
        .expect("physical close");
}

#[tokio::test]
async fn pty_interrupt_bypasses_blocked_output_and_close_restores_terminal() {
    let pty = nix::pty::openpty(None, None).expect("PTY");
    let original = rustix::termios::tcgetattr(&pty.slave).expect("original terminal");
    let flags = rustix::fs::fcntl_getfl(&pty.slave).expect("original flags");
    let (output, _blocked_peer) = UnixStream::pair().expect("blocked output socket");
    let active = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("scope");
    let (_input, mut interrupts, mut terminal) = Terminal::spawn(
        duplicate(&pty.slave).expect("stdin"),
        duplicate(&output).expect("stdout"),
        active,
    )
    .await
    .expect("worker");
    tokio::time::timeout(Duration::from_secs(2), async {
        while rustix::termios::tcgetattr(&pty.slave)
            .expect("terminal")
            .local_modes
            .contains(rustix::termios::LocalModes::ICANON)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded noncanonical input enabled");
    {
        let writing = terminal.print("output".repeat(1024 * 1024));
        tokio::pin!(writing);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut writing)
                .await
                .is_err()
        );
        rustix::io::write(&pty.master, b"\x03").expect("real terminal Ctrl-C");
        tokio::time::timeout(Duration::from_secs(2), interrupts.recv())
            .await
            .expect("Ctrl-C bypasses stdout")
            .expect("interrupt signal");
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut writing)
                .await
                .is_err()
        );
    }
    terminal.close().await.expect("shutdown wakes the worker");
    assert_eq!(terminal.output_slot.available_permits(), 1);
    assert_eq!(
        rustix::termios::tcgetattr(&pty.slave)
            .expect("restored terminal")
            .local_modes,
        original.local_modes
    );
    assert_eq!(
        rustix::fs::fcntl_getfl(&pty.slave).expect("restored flags"),
        flags
    );
}

#[tokio::test]
async fn full_input_queue_refuses_while_output_is_blocked_and_worker_exits() {
    let (mut input, _interrupts, mut terminal, mut input_peer, _output_peer) = fixture().await;
    {
        let writing = terminal.print("output".repeat(1024 * 1024));
        tokio::pin!(writing);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut writing)
                .await
                .is_err()
        );
        input_peer
            .write_all(
                "queued\n"
                    .repeat(rw_types::MAX_CLIENT_CONTROLS + 1)
                    .as_bytes(),
            )
            .expect("overflow input");
        let error = tokio::time::timeout(Duration::from_secs(2), &mut writing)
            .await
            .expect("refusal releases print")
            .expect_err("explicit input refusal");
        assert!(error.to_string().contains("queue is full"));
    }
    assert!(
        matches!(input.recv().await.expect("refusal").value, InputLine::Error(message) if message.contains("queue is full"))
    );
    assert!(
        terminal
            .close()
            .await
            .expect_err("physical worker refusal")
            .to_string()
            .contains("queue is full")
    );
    assert_eq!(terminal.output_slot.available_permits(), 1);
}

#[tokio::test]
async fn output_stall_has_a_finite_refusal_and_restores_descriptor_flags() {
    let (_input, _interrupts, mut terminal, _input_peer, _output_peer) = fixture().await;
    let error = tokio::time::timeout(
        Duration::from_secs(8),
        terminal.print("output".repeat(1024 * 1024)),
    )
    .await
    .expect("bounded physical stall")
    .expect_err("blocked output refusal");
    assert!(error.to_string().contains("output failed"));
    assert!(
        terminal
            .close()
            .await
            .expect_err("source stall error")
            .to_string()
            .contains("remained blocked")
    );
    assert_eq!(terminal.output_slot.available_permits(), 1);
}

#[tokio::test]
async fn cancelling_close_retains_receipt_until_physical_finalization() {
    let (input, _input_peer) = UnixStream::pair().expect("input");
    let (output, _output_peer) = UnixStream::pair().expect("output");
    let budget = Arc::new(Semaphore::new(1));
    let active = budget.clone().try_acquire_owned().expect("scope");
    let (entered, finalizing) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let (_receive, _interrupts, mut terminal) = Terminal::spawn_with_finalizer(
        duplicate(&input).expect("input descriptor"),
        duplicate(&output).expect("output descriptor"),
        active,
        move || {
            let _ = entered.send(());
            wait.recv_timeout(Duration::from_secs(5))
                .expect("release physical finalizer");
        },
    )
    .await
    .expect("worker");
    {
        let closing = terminal.close();
        tokio::pin!(closing);
        tokio::select! {
            entered = finalizing => entered.expect("worker owns pending finalization"),
            result = &mut closing => panic!("close finished before physical finalization: {result:?}"),
        }
    }
    assert_eq!(budget.available_permits(), 0);
    assert!(terminal.finished.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), terminal.close())
            .await
            .is_err()
    );
    release.send(()).expect("settle worker");
    terminal
        .close()
        .await
        .expect("retry observes actual completion");
    assert_eq!(budget.available_permits(), 1);
    assert!(terminal.finished.is_none());
}

#[tokio::test]
async fn output_only_caller_loss_retains_slot_until_wake_and_restores_shared_flags() {
    let (stdout, _blocked_peer) = UnixStream::pair().expect("stdout");
    let flags = rustix::fs::fcntl_getfl(&stdout).expect("flags");
    let active = Arc::new(Semaphore::new(1));
    let (_, _, mut terminal) = Terminal::spawn_mode(
        super::Descriptors::Output {
            stdout: duplicate(&stdout).expect("stdout"),
            stderr: duplicate(&stdout).expect("stderr shares file description"),
        },
        active.clone().try_acquire_owned().expect("admission"),
        || {},
    )
    .await
    .expect("output-only worker");
    {
        let write = terminal.print("x".repeat(1024 * 1024));
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut write)
                .await
                .is_err()
        );
    }
    assert_eq!(terminal.output_slot.available_permits(), 0);
    assert_eq!(active.available_permits(), 0);
    assert!(terminal.print("second".to_owned()).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), terminal.close())
        .await
        .expect("wake")
        .expect("settle");
    assert_eq!(terminal.output_slot.available_permits(), 1);
    assert_eq!(active.available_permits(), 1);
    // Writes may set kernel bookkeeping bits. The owned status flags must
    // return to their original values, including the shared nonblocking mode.
    let status = rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::APPEND
        | rustix::fs::OFlags::ACCMODE;
    assert_eq!(
        rustix::fs::fcntl_getfl(&stdout).expect("restored flags") & status,
        flags & status
    );
}

#[tokio::test]
async fn output_only_preserves_stdout_and_stderr_without_prompt_or_input() {
    let (stdout, mut out) = UnixStream::pair().expect("stdout");
    let (stderr, mut err) = UnixStream::pair().expect("stderr");
    let active = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("admission");
    let (_, _, mut terminal) = Terminal::spawn_mode(
        super::Descriptors::Output {
            stdout: stdout.into(),
            stderr: stderr.into(),
        },
        active,
        || {},
    )
    .await
    .expect("output-only worker");
    terminal.print("héllo\n".to_owned()).await.expect("stdout");
    terminal
        .print_to("error\n".to_owned(), true)
        .await
        .expect("stderr");
    terminal.close().await.expect("settle");
    let mut normal = String::new();
    let mut diagnostic = String::new();
    out.read_to_string(&mut normal).expect("stdout EOF");
    err.read_to_string(&mut diagnostic).expect("stderr EOF");
    assert_eq!(normal, "héllo\n");
    assert_eq!(diagnostic, "error\n");
}
