fn main() {
    match rw_sandbox::maybe_run_helper(std::env::args_os()) {
        Ok(false) => std::process::exit(2),
        Ok(true) => unreachable!("sandbox helper execution never returns"),
        Err(error) => {
            eprintln!("sandbox helper failed: {error}");
            std::process::exit(125);
        }
    }
}
