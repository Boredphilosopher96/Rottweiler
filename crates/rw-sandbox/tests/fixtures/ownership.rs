//! Non-shipped native process for attested-data and physical-retirement tests.
use std::io::{self, Read as _, Write as _};

fn run() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let input = arguments
        .next()
        .ok_or_else(|| io::Error::other("one attested input file is required"))?;
    if arguments.next().is_some() {
        return Err(io::Error::other("unexpected argument"));
    }
    match std::fs::metadata("unlisted") {
        Ok(_) => return Err(io::Error::other("unlisted file became readable")),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) => {}
        Err(error) => return Err(error),
    }
    let mut data = Vec::new();
    std::fs::File::open(input)?
        .take(129)
        .read_to_end(&mut data)?;
    match data.as_slice() {
        b"approved\n" => io::stdout().write_all(b"approved"),
        b"hold\n" => {
            io::stdout().write_all(b"ready")?;
            io::stdout().flush()?;
            loop {
                std::thread::park();
            }
        }
        _ => Err(io::Error::other("invalid or oversized attested input")),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ownership fixture: {error}");
        std::process::exit(125);
    }
}
