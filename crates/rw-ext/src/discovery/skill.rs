//! Discovered SKILL.md skills: metadata, instructions, and bundled files.
//!
//! A skill is anchored at its canonical directory. Invocation re-reads
//! SKILL.md so edits take effect without rediscovery, lists a bounded sample
//! of bundled files, and leaves those files for the model to request on
//! demand. Bundle size never makes a skill fail.

use std::collections::VecDeque;
use std::fmt::Write as _;

use super::{
    ArtifactOrigin, ExtensionDiscoveryError, MAX_MARKDOWN_BYTES, Path, PathBuf, fs,
    parse_frontmatter, read_bounded_relative_file, validate_relative_resource,
};

/// Bundled files listed in an invocation.
pub const SKILL_BUNDLE_LISTING_LIMIT: usize = 64;
/// Largest bundled file returned by [`DiscoveredSkill::read_bundled_file`].
pub const MAX_SKILL_BUNDLED_FILE_BYTES: u64 = 256 * 1024;
/// Directory entries inspected while listing one bundle.
const MAX_BUNDLE_ENTRIES_VISITED: usize = 4_096;
const SKILL_DIR_PLACEHOLDER: &str = "${CLAUDE_SKILL_DIR}";

/// SKILL.md metadata. Instructions and bundled files stay lazy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredSkill {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) allowed_tools: Vec<String>,
    pub(super) origin: ArtifactOrigin,
    pub(super) root: PathBuf,
}

/// Why a bundle entry was left out of a listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkippedBundleReason {
    SymbolicLink,
    HiddenOrDependencyDirectory,
    Unreadable,
    UnsupportedEntry,
}

impl SkippedBundleReason {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::SymbolicLink => "symbolic link not followed",
            Self::HiddenOrDependencyDirectory => "hidden or dependency directory not listed",
            Self::Unreadable => "unreadable",
            Self::UnsupportedEntry => "not a regular file or directory",
        }
    }
}

/// One entry left out of a bundle listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedBundleEntry {
    pub relative_path: PathBuf,
    pub reason: SkippedBundleReason,
}

/// Bounded view of the files bundled beside SKILL.md.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillBundleListing {
    /// Relative paths in breadth-first, name-sorted order.
    pub files: Vec<PathBuf>,
    /// More files exist than were listed.
    pub truncated: bool,
    pub skipped: Vec<SkippedBundleEntry>,
}

impl DiscoveredSkill {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }

    /// Canonical skill directory. Bundled paths are relative to it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads the current SKILL.md instruction body. Edits made after
    /// discovery are picked up; metadata stays as discovered.
    ///
    /// # Errors
    ///
    /// Fails when SKILL.md was removed, replaced by a link, grew past the size
    /// limit, is not UTF-8, or no longer has valid frontmatter.
    pub fn load_instructions(&self) -> Result<String, ExtensionDiscoveryError> {
        let path = self.root.join("SKILL.md");
        let bytes =
            read_bounded_relative_file(&self.root, Path::new("SKILL.md"), MAX_MARKDOWN_BYTES)?;
        let contents = String::from_utf8(bytes)
            .map_err(|_| ExtensionDiscoveryError::NotUtf8 { path: path.clone() })?;
        let document = parse_frontmatter(&path, &contents)?;
        Ok(document.body.to_owned())
    }

    /// Lists at most `limit` bundled files without reading them. Symbolic
    /// links, hidden directories, and `node_modules` are skipped and reported;
    /// the walk itself is bounded, so a large bundle never fails a skill.
    #[must_use]
    pub fn bundle_listing(&self, limit: usize) -> SkillBundleListing {
        let mut listing = SkillBundleListing::default();
        let mut queue = VecDeque::from([PathBuf::new()]);
        let mut visited = 0_usize;
        while let Some(relative) = queue.pop_front() {
            let Ok(entries) = fs::read_dir(self.root.join(&relative)) else {
                if !relative.as_os_str().is_empty() {
                    listing.skipped.push(SkippedBundleEntry {
                        relative_path: relative,
                        reason: SkippedBundleReason::Unreadable,
                    });
                }
                continue;
            };
            let mut names = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>();
            names.sort();
            for name in names {
                visited += 1;
                if visited > MAX_BUNDLE_ENTRIES_VISITED {
                    listing.truncated = true;
                    return listing;
                }
                let child = relative.join(&name);
                if child == Path::new("SKILL.md") {
                    continue;
                }
                let Ok(metadata) = fs::symlink_metadata(self.root.join(&child)) else {
                    listing.skipped.push(SkippedBundleEntry {
                        relative_path: child,
                        reason: SkippedBundleReason::Unreadable,
                    });
                    continue;
                };
                let skipped = if metadata.file_type().is_symlink() {
                    Some(SkippedBundleReason::SymbolicLink)
                } else if metadata.is_dir() {
                    let hidden = name
                        .to_str()
                        .is_none_or(|name| name.starts_with('.') || name == "node_modules");
                    if hidden {
                        Some(SkippedBundleReason::HiddenOrDependencyDirectory)
                    } else {
                        queue.push_back(child.clone());
                        None
                    }
                } else if metadata.is_file() {
                    if listing.files.len() >= limit {
                        listing.truncated = true;
                        return listing;
                    }
                    listing.files.push(child.clone());
                    None
                } else {
                    Some(SkippedBundleReason::UnsupportedEntry)
                };
                if let Some(reason) = skipped {
                    listing.skipped.push(SkippedBundleEntry {
                        relative_path: child,
                        reason,
                    });
                }
            }
        }
        listing
    }

    /// Reads one bundled UTF-8 file relative to the skill root without
    /// following links.
    ///
    /// # Errors
    ///
    /// Rejects absolute or parent-traversing paths, links, directories,
    /// binary content, and files above [`MAX_SKILL_BUNDLED_FILE_BYTES`].
    pub fn read_bundled_file(&self, relative: &str) -> Result<String, ExtensionDiscoveryError> {
        let relative = Path::new(relative);
        validate_relative_resource(relative)?;
        let bytes = read_bounded_relative_file(&self.root, relative, MAX_SKILL_BUNDLED_FILE_BYTES)?;
        String::from_utf8(bytes).map_err(|_| ExtensionDiscoveryError::NotUtf8 {
            path: self.root.join(relative),
        })
    }

    /// Instructions delivered when the user or the model invokes this skill:
    /// the SKILL.md body, the skill root, and a bounded bundled-file listing.
    ///
    /// # Errors
    ///
    /// Fails only when SKILL.md itself can no longer be read; see
    /// [`Self::load_instructions`].
    pub fn render_invocation(&self, arguments: &str) -> Result<String, ExtensionDiscoveryError> {
        let root = self.root.display().to_string();
        let body = self
            .load_instructions()?
            .replace(SKILL_DIR_PLACEHOLDER, &root);
        let listing = self.bundle_listing(SKILL_BUNDLE_LISTING_LIMIT);
        let mut rendered = format!("# Skill: {}\n\nSkill root: {root}\n\n", self.name);
        rendered.push_str(body.trim_end());
        rendered.push('\n');
        if !listing.files.is_empty() {
            let _ = write!(
                rendered,
                "\n## Bundled files\n\nPaths are relative to the skill root. Load one with the `skill` tool, for example {{\"name\": \"{}\", \"path\": \"{}\"}}.\n",
                self.name,
                listing.files[0].display()
            );
            for file in &listing.files {
                let _ = writeln!(rendered, "- {}", file.display());
            }
            if listing.truncated {
                rendered.push_str("- … more files not listed\n");
            }
        }
        if !listing.skipped.is_empty() {
            let _ = writeln!(
                rendered,
                "\n{} bundle entries were not listed (symbolic links, hidden or dependency directories, or unreadable entries).",
                listing.skipped.len()
            );
        }
        if !arguments.trim().is_empty() {
            rendered.push_str("\n## Invocation arguments\n\n");
            rendered.push_str(arguments);
            rendered.push('\n');
        }
        Ok(rendered)
    }
}
