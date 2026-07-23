//! Deterministic plugin authoring CLI surfaces.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic, Result, miette};

struct TemplateFile {
    path: &'static str,
    contents: String,
}

pub(crate) fn scaffold_typescript(
    destination: &Path,
    name: Option<&str>,
    force: bool,
) -> Result<Vec<PathBuf>> {
    let name = package_name(name.unwrap_or("rottweiler-plugin"))?;
    let files = typescript_template(&name);
    let root = prepare_root(destination)?;
    for file in &files {
        let target = root.join(file.path);
        ensure_beneath(&root, &target)?;
        if let Ok(metadata) = fs::symlink_metadata(&target) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(miette!(
                    "refusing to replace non-regular scaffold target {}",
                    target.display()
                ));
            }
            if !force {
                return Err(miette!(
                    "scaffold target already exists: {}",
                    target.display()
                ));
            }
        }
    }
    let mut written = Vec::new();
    for file in files {
        let target = root.join(file.path);
        let parent = target
            .parent()
            .ok_or_else(|| miette!("invalid scaffold target"))?;
        fs::create_dir_all(parent).into_diagnostic()?;
        if fs::symlink_metadata(parent)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return Err(miette!("scaffold parent is not a real directory"));
        }
        atomic_write(&target, file.contents.as_bytes())?;
        written.push(target);
    }
    Ok(written)
}

fn prepare_root(destination: &Path) -> Result<PathBuf> {
    let absolute = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir().into_diagnostic()?.join(destination)
    };
    if let Ok(metadata) = fs::symlink_metadata(&absolute) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(miette!("scaffold destination must be a real directory"));
        }
        return fs::canonicalize(absolute).into_diagnostic();
    }
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| miette!("scaffold destination has no existing ancestor"))?;
    }
    let canonical_ancestor = fs::canonicalize(ancestor).into_diagnostic()?;
    let suffix = absolute.strip_prefix(ancestor).into_diagnostic()?;
    let root = canonical_ancestor.join(suffix);
    fs::create_dir_all(&root).into_diagnostic()?;
    fs::canonicalize(root).into_diagnostic()
}

fn ensure_beneath(root: &Path, target: &Path) -> Result<()> {
    if !target.starts_with(root) {
        return Err(miette!("scaffold template escaped destination"));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| miette!("scaffold target has no parent"))?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| miette!("scaffold entropy failed: {error}"))?;
    let temp = parent.join(format!(
        ".rottweiler-scaffold-{}.tmp",
        u64::from_ne_bytes(random)
    ));
    let cleanup = TempCleanup(temp.clone());
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o644);
    }
    let mut file = options.open(&temp).into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    fs::rename(&temp, path).into_diagnostic()?;
    std::mem::forget(cleanup);
    Ok(())
}

struct TempCleanup(PathBuf);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn package_name(value: &str) -> Result<String> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized
        .trim_matches('-')
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        return Err(miette!("plugin name must contain a letter or number"));
    }
    Ok(normalized)
}

fn typescript_template(name: &str) -> Vec<TemplateFile> {
    const MARKER: &str = "__ROTTWEILER_PLUGIN_NAME__";
    [
        (
            "package.json",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/package.json"),
        ),
        (
            "tsconfig.json",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/tsconfig.json"),
        ),
        (
            "manifest.json",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/manifest.json"),
        ),
        (
            "src/index.ts",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/src/index.ts"),
        ),
        (
            "test/plugin.test.ts",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/test/plugin.test.ts"),
        ),
        (
            ".gitignore",
            include_str!("../../../packages/plugin-sdk/fixtures/scaffold/gitignore"),
        ),
    ]
    .into_iter()
    .map(|(path, contents)| TemplateFile {
        path,
        contents: contents.replace(MARKER, name),
    })
    .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn scaffold_is_deterministic_and_refuses_overwrite() {
        let root = tempfile::tempdir().expect("root");
        let destination = root.path().join("demo");
        let first = scaffold_typescript(&destination, Some("My Plugin"), false).expect("scaffold");
        assert_eq!(first.len(), 6);
        assert!(
            fs::read_to_string(destination.join("package.json"))
                .expect("package")
                .contains("my-plugin")
        );
        let manifest = rw_ext::PluginManifest::from_slice(
            &fs::read(destination.join("manifest.json")).expect("manifest"),
        )
        .expect("trusted manifest parses");
        assert_eq!(manifest.name, "my-plugin");
        assert!(scaffold_typescript(&destination, Some("My Plugin"), false).is_err());
        let before = fs::read(destination.join("src/index.ts")).expect("before");
        scaffold_typescript(&destination, Some("My Plugin"), true).expect("force");
        assert_eq!(
            before,
            fs::read(destination.join("src/index.ts")).expect("after")
        );
        assert!(fs::read_dir(&destination).expect("list").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));
    }
}
