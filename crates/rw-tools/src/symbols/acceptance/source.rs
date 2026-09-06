//! Deterministic bounded source generation and an independent definition oracle.
use super::*;
use std::fs;

pub(super) const FILE_BYTES: usize = 4096;
pub(super) const DEFINITIONS: usize = 8;

pub(super) struct Repository {
    pub root: tempfile::TempDir,
    pub files: usize,
    pub digest: String,
}

impl Repository {
    pub fn seed(files: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let mut repository = Self {
            root: tempfile::tempdir()?,
            files,
            digest: String::new(),
        };
        let mut digest = blake3::Hasher::new();
        for file in 0..files {
            repository.write(file, 'a')?;
            let path = Self::path(file);
            digest.update(path.to_string_lossy().as_bytes());
            digest.update(&fs::read(repository.root.path().join(path))?);
        }
        repository.digest = digest.finalize().to_hex().to_string();
        Ok(repository)
    }

    pub fn path(file: usize) -> PathBuf {
        let extension = ["rs", "py", "ts"][file % 3];
        PathBuf::from(format!(
            "src/group_{:03}/file_{file:05}.{extension}",
            file / 100
        ))
    }

    pub fn name(file: usize, revision: char, slot: usize) -> String {
        format!("hound_{file:05}_{revision}_{slot}")
    }

    pub fn write(&self, file: usize, revision: char) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.root.path().join(Self::path(file));
        fs::create_dir_all(path.parent().ok_or("source parent")?)?;
        let mut source = String::with_capacity(FILE_BYTES);
        for slot in 0..DEFINITIONS {
            let name = Self::name(file, revision, slot);
            source.push_str(&match file % 3 {
                0 => format!("pub fn {name}() -> u64 {{ 7 }}\n"),
                1 => format!("def {name}():\n    return 7\n"),
                _ => format!("export function {name}(): number {{ return 7; }}\n"),
            });
        }
        source.push_str(if file % 3 == 1 { "#" } else { "//" });
        while source.len() < FILE_BYTES - 1 {
            source.push('x');
        }
        source.push('\n');
        assert_eq!(source.len(), FILE_BYTES);
        fs::write(path, source)?;
        Ok(())
    }

    pub fn remove(&self, file: usize) -> Result<(), Box<dyn std::error::Error>> {
        fs::remove_file(self.root.path().join(Self::path(file)))?;
        Ok(())
    }

    pub fn verify_file(
        &self,
        index: &WorkspaceSymbolIndex,
        file: usize,
        revision: Option<char>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let symbols = index.symbols_for_file(Self::path(file))?;
        let mut actual = symbols
            .iter()
            .filter(|symbol| symbol.role == SymbolRole::Definition)
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        actual.sort_unstable();
        let expected = revision.map_or_else(Vec::new, |revision| {
            (0..DEFINITIONS)
                .map(|slot| Self::name(file, revision, slot))
                .collect::<Vec<_>>()
        });
        assert_eq!(actual, expected, "file {file}");
        for symbol in symbols
            .iter()
            .filter(|symbol| symbol.role == SymbolRole::Definition)
        {
            assert_eq!(symbol.location.path, Self::path(file));
            let slot = symbol
                .name
                .rsplit('_')
                .next()
                .ok_or("symbol slot")?
                .parse::<usize>()?;
            let line = if file % 3 == 1 {
                slot * 2 + 1
            } else {
                slot + 1
            };
            let column = [8, 5, 17][file % 3];
            assert_eq!(symbol.kind, rw_intel::SymbolKind::Function);
            assert_eq!(symbol.location.line, line);
            assert_eq!(symbol.location.end_line, line);
            assert_eq!(symbol.location.column, column);
            assert_eq!(symbol.location.end_column, column + symbol.name.len());
        }
        Ok(())
    }

    pub fn verify_all(
        &self,
        index: &WorkspaceSymbolIndex,
        replacement: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut digest = blake3::Hasher::new();
        for file in 0..self.files {
            let revision = if file < replacement { 'b' } else { 'a' };
            self.verify_file(index, file, Some(revision))?;
            for slot in 0..DEFINITIONS {
                digest.update(Self::name(file, revision, slot).as_bytes());
                digest.update(b"\n");
            }
        }
        Ok(digest.finalize().to_hex().to_string())
    }
}
