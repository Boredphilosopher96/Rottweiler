use std::{
    fs::File,
    io::Read as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub struct Output {
    pub success: bool,
    pub text: String,
    pub elapsed: Duration,
}
pub fn run(mut command: Command) -> Output {
    let stdout = tempfile::tempfile().expect("stdout owner");
    let stderr = tempfile::tempfile().expect("stderr owner");
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().expect("stdout"))
        .stderr(stderr.try_clone().expect("stderr"));
    let started = Instant::now();
    let mut process =
        rw_resources::process::BlockingProcess::spawn(&mut command).expect("native candidate");
    let status = loop {
        if let Some(status) = process
            .child_mut()
            .expect("child owner")
            .try_wait()
            .expect("status")
        {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(40)
            || stdout.metadata().expect("output size").len()
                + stderr.metadata().expect("error size").len()
                > 512 * 1024
        {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    process.settle();
    let mut text = read(stdout);
    text.push_str(&read(stderr));
    assert!(
        status.is_some(),
        "candidate exceeded its bounded execution/output policy: {text}"
    );
    Output {
        success: status.expect("status").success(),
        text,
        elapsed: started.elapsed(),
    }
}
fn read(mut file: File) -> String {
    use std::io::Seek as _;
    file.rewind().expect("output start");
    let mut bytes = Vec::new();
    file.take(512 * 1024)
        .read_to_end(&mut bytes)
        .expect("bounded output");
    String::from_utf8_lossy(&bytes).into_owned()
}
