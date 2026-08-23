use std::{
    env,
    fmt::Write as _,
    fs, io,
    path::{Component, Path, PathBuf},
};

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crate_root = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| invalid_data("missing crate root"))?,
    );
    let scaffold_root = crate_root.join("../../packages/plugin-sdk/fixtures/scaffold");
    let manifest = scaffold_root.join("files.txt");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let mut generated = String::from("const TYPESCRIPT_SCAFFOLD: &[(&str, &str)] = &[\n");
    let mappings = fs::read_to_string(&manifest)?;
    for line in mappings.lines() {
        let (source, destination) = line
            .split_once('\t')
            .ok_or_else(|| invalid_data("scaffold mappings require a tab separator"))?;
        if destination.contains('\t') || !safe_relative(source) || !safe_relative(destination) {
            return Err(invalid_data("scaffold mappings require safe relative paths").into());
        }

        let source_path = scaffold_root.join(source);
        println!("cargo:rerun-if-changed={}", source_path.display());
        let contents = fs::read_to_string(source_path)?;
        writeln!(generated, "    ({destination:?}, {contents:?}),")?;
    }
    generated.push_str("];\n");

    let output =
        PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| invalid_data("missing build output"))?)
            .join("typescript_scaffold.rs");
    fs::write(output, generated)?;
    Ok(())
}
