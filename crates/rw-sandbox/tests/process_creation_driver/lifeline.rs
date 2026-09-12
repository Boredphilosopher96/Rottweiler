//! A native single-process sandbox can change groups; only the receipt proves retirement.
use rustix::process::Pid;
use rw_sandbox::{
    NetworkPolicy, PluginLifeline, PluginRendezvous, SandboxPolicy, shell_launch_plan,
};
use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    os::unix::process::CommandExt as _,
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

pub(super) fn escape() {
    rustix::process::setsid().expect("single-process sandbox permits session change");
    println!("{}", std::process::id());
    std::io::stdout().flush().expect("ready");
    // This independent fixture lifeline is held by the test, not the helper.
    // The negative assertion cannot strand the escaped process on panic/drop.
    let mut byte = [0];
    while std::io::stdin()
        .read(&mut byte)
        .expect("fixture cleanup lifeline")
        != 0
    {}
}

struct Fixture {
    helper: Child,
    input: Option<ChildStdin>,
    control: PluginLifeline,
    effect: Option<Pid>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.control.stop();
        // No signal targets a remembered effect PID. Its owned input EOF is
        // the cleanup trigger; the helper is still our actual waitable child.
        self.helper.wait().expect("helper retirement");
        if let Some(pid) = self.effect {
            let deadline = Instant::now() + Duration::from_secs(5);
            while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
                assert!(
                    Instant::now() < deadline,
                    "escaped fixture retirement was not proved"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

pub(super) fn verify() {
    let helper = super::common::helper();
    let scratch = tempfile::tempdir().expect("scratch");
    let policy = SandboxPolicy::new([scratch.path()], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let mut plan = shell_launch_plan(
        &policy,
        &helper,
        &std::env::current_exe().expect("fixture"),
        &["--lifeline-escape".into()],
    )
    .expect("single-process sandbox");
    let rendezvous = PluginRendezvous::bind().expect("rendezvous");
    rendezvous.wrap(&mut plan).expect("supervisor");
    let mut child = Command::new(&plan.program)
        .args(&plan.args)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("supervisor");
    let control = rendezvous
        .accept(
            child.id(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            &|| false,
        )
        .expect("verified supervisor");
    let input = child.stdin.take();
    let mut fixture = Fixture {
        helper: child,
        input,
        control,
        effect: None,
    };
    fixture.control.grant().expect("grant");
    let mut ready = String::new();
    BufReader::new(fixture.helper.stdout.take().expect("stdout"))
        .read_line(&mut ready)
        .expect("ready");
    let effect = Pid::from_raw(ready.trim().parse().expect("effect pid")).expect("pid");
    fixture.effect = Some(effect);
    assert_eq!(
        rustix::process::getpgid(Some(effect)).expect("actual group"),
        effect
    );
    let group = Pid::from_child(&fixture.helper);
    assert_ne!(group, effect);
    fixture.helper.kill().expect("kill only trusted supervisor");
    fixture.helper.wait().expect("reap supervisor");
    assert_eq!(
        rustix::process::test_kill_process_group(group),
        Err(rustix::io::Errno::SRCH)
    );
    assert!(
        rustix::process::test_kill_process(effect).is_ok(),
        "effect survives in its new group"
    );
    assert!(
        fixture.control.verify_settlement().is_err(),
        "old group absence is not a receipt"
    );
    assert!(
        fixture.control.verify_settlement().is_err(),
        "failure stays unsettled"
    );
    drop(fixture);
}
