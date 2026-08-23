//! Deterministic plugin authoring CLI surfaces.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use miette::{IntoDiagnostic, Result, miette};

struct TemplateFile {
    path: &'static str,
    contents: String,
}

include!(concat!(env!("OUT_DIR"), "/typescript_scaffold.rs"));

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

pub(crate) fn check_typescript(source: &Path) -> Result<()> {
    let root = canonical_project_root(source)?;
    let manifest = read_regular_file(&root, "manifest.json")?;
    let manifest = rw_plugin_protocol::PluginManifest::from_slice(&manifest)
        .map_err(|error| miette!("plugin manifest is invalid: {error}"))?;
    let package = read_regular_file(&root, "package.json")?;
    let package: serde_json::Value = serde_json::from_slice(&package).into_diagnostic()?;
    let package_name = package.get("name").and_then(serde_json::Value::as_str);
    if package_name != Some(manifest.name.as_str()) {
        return Err(miette!(
            "package name and manifest name must match exactly (manifest: {:?}, package: {:?})",
            manifest.name,
            package_name
        ));
    }
    for script in ["typecheck", "test"] {
        if package
            .pointer(&format!("/scripts/{script}"))
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(miette!(
                "package.json must define a non-empty {script} script"
            ));
        }
        run_bun_script(&root, script)?;
    }
    Ok(())
}

fn canonical_project_root(source: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(source).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(miette!("plugin path must be a real directory"));
    }
    fs::canonicalize(source).into_diagnostic()
}

fn read_regular_file(root: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(miette!("plugin {relative} must be a regular file"));
    }
    fs::read(path).into_diagnostic()
}

fn run_bun_script(root: &Path, script: &str) -> Result<()> {
    let status = Command::new("bun")
        .args(["run", script])
        .current_dir(root)
        .env("CI", "1")
        .status()
        .into_diagnostic()?;
    if status.success() {
        Ok(())
    } else {
        Err(miette!("plugin {script} failed with {status}"))
    }
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
    TYPESCRIPT_SCAFFOLD
        .iter()
        .copied()
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
        let manifest = rw_plugin_protocol::PluginManifest::from_slice(
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

    #[test]
    fn check_rejects_package_manifest_identity_drift_before_execution() {
        let root = tempfile::tempdir().expect("root");
        scaffold_typescript(root.path(), Some("manifest-name"), false).expect("scaffold");
        let package = root.path().join("package.json");
        let changed = fs::read_to_string(&package)
            .expect("package")
            .replace("manifest-name", "different-name");
        fs::write(package, changed).expect("rewrite fixture");
        let error = check_typescript(root.path()).expect_err("identity drift must fail");
        assert!(error.to_string().contains("must match exactly"));
    }
}
