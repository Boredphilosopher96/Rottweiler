mod git;
mod search;
pub(super) use git::{hardened_git_subcommand, safe_git_arguments};
pub(super) use search::{audited_rg, safe_search_arguments};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use globset::{GlobBuilder, GlobMatcher};

/// Conservative built-in safe-list result used by the permission chokepoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSafety {
    /// The entire command is a recognized read-only operation and may run
    /// without a prompt, but still inside the OS sandbox.
    SafeListed,
    /// The command is unknown, compound, interpolated, or potentially
    /// mutating.  Normal permission policy applies.
    RequiresApproval,
}

/// One immutable command classifier shared by permission policy and execution.
/// User patterns are accepted only from the already-filtered user config layer.
#[derive(Clone, Debug, Default)]
pub struct CommandSafetyClassifier {
    user_patterns: Vec<GlobMatcher>,
}

impl CommandSafetyClassifier {
    /// Compiles user-scoped command globs. Invalid patterns fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured glob cannot be compiled.
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let user_patterns = patterns
            .iter()
            .map(|pattern| {
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| {
                        format!("invalid sandbox safe-list pattern {pattern:?}: {error}")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { user_patterns })
    }

    #[must_use]
    pub fn classify(&self, command: &str) -> CommandSafety {
        let Some(segments) = safe_command_segments(command) else {
            return CommandSafety::RequiresApproval;
        };
        if segments.iter().all(|(segment, _)| {
            built_in_safe_segment(segment)
                || self
                    .user_patterns
                    .iter()
                    .any(|pattern| pattern.is_match(segment))
        }) {
            CommandSafety::SafeListed
        } else {
            CommandSafety::RequiresApproval
        }
    }
}

/// Classifies a canonical shell command for the built-in no-prompt safe-list.
///
/// This list is intentionally small.  Shell interpolation and control syntax
/// are rejected before tokenization; each `&&`/`||`/`;` segment must be an
/// audited read-only command, such as a read-only `git` form (see `git.rs`).  A user may extend the
/// safe-list through user-scoped permission configuration; project content
/// never calls this function with additional rules.
#[must_use]
pub fn classify_safe_command(command: &str) -> CommandSafety {
    CommandSafetyClassifier::default().classify(command)
}

pub(super) fn built_in_safe_segment(command: &str) -> bool {
    let Ok(argv) = shell_words::split(command) else {
        return false;
    };
    match argv.first().map(String::as_str) {
        Some("git") if audited_system_git().is_some() => argv
            .get(1)
            .is_some_and(|subcommand| safe_git_arguments(subcommand, &argv[2..])),
        Some("cat") => audited_system_read_command("cat").is_some(),
        Some("ls") => audited_system_read_command("ls").is_some(),
        Some(name @ ("grep" | "find")) => {
            audited_system_read_command(name).is_some() && safe_search_arguments(name, &argv[1..])
        }
        Some("rg") => audited_rg().is_some() && safe_search_arguments("rg", &argv[1..]),
        Some("bat") => audited_bat().is_some() && safe_bat_arguments(&argv[1..]),
        _ => false,
    }
}

pub(super) fn safe_bat_arguments(arguments: &[String]) -> bool {
    !arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "--pager" | "-P" | "--paging" | "--diff" | "-d" | "--config-file"
        ) || argument.starts_with("--pager=")
            || argument.starts_with("--paging=")
            || argument.starts_with("--config-file=")
    })
}

pub(super) fn safe_command_segments(command: &str) -> Option<Vec<(String, Option<String>)>> {
    if command.is_empty()
        || command.contains(['\n', '\r', '`', '$'])
        || command.as_bytes().contains(&0)
    {
        return None;
    }
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        match character {
            '\\' if !single => {
                escaped = true;
                index += 1;
                continue;
            }
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
        if !single && !double {
            let next = chars.get(index + 1).map(|(_, next)| *next);
            let delimiter = match (character, next) {
                ('&', Some('&')) => Some((2, "&&")),
                ('|', Some('|')) => Some((2, "||")),
                (';', _) => Some((1, ";")),
                ('|' | '&' | '<' | '>' | '(' | ')', _) => return None,
                _ => None,
            };
            if let Some((delimiter_len, operator)) = delimiter {
                let segment = command.get(start..offset)?.trim();
                let canonical = canonical_segment(segment)?;
                if canonical.is_empty() {
                    return None;
                }
                segments.push((canonical, Some(operator.to_owned())));
                index += delimiter_len;
                start = chars.get(index).map_or(command.len(), |(next, _)| *next);
                continue;
            }
        }
        index += 1;
    }
    if single || double || escaped {
        return None;
    }
    let canonical = canonical_segment(command.get(start..)?.trim())?;
    if canonical.is_empty() {
        return None;
    }
    segments.push((canonical, None));
    Some(segments)
}

fn canonical_segment(segment: &str) -> Option<String> {
    Some(
        shell_words::split(segment)
            .ok()?
            .iter()
            .map(|argument| shell_words::quote(argument).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub(super) fn audited_system_read_command(name: &str) -> Option<&'static PathBuf> {
    static CAT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static LS: OnceLock<Option<PathBuf>> = OnceLock::new();
    static GREP: OnceLock<Option<PathBuf>> = OnceLock::new();
    static FIND: OnceLock<Option<PathBuf>> = OnceLock::new();
    let (slot, candidates): (&OnceLock<Option<PathBuf>>, &[&str]) = match name {
        "cat" => (&CAT, &["/bin/cat", "/usr/bin/cat"]),
        "ls" => (&LS, &["/bin/ls", "/usr/bin/ls"]),
        "grep" => (&GREP, &["/usr/bin/grep", "/bin/grep"]),
        "find" => (&FIND, &["/usr/bin/find", "/bin/find"]),
        _ => return None,
    };
    slot.get_or_init(|| resolve_audited_system_binary(candidates))
        .as_ref()
}

pub(super) fn audited_bat() -> Option<&'static PathBuf> {
    static BAT: OnceLock<Option<PathBuf>> = OnceLock::new();
    BAT.get_or_init(|| {
        resolve_audited_local_binary(&[
            "/opt/homebrew/bin/bat",
            "/usr/local/bin/bat",
            "/usr/bin/bat",
            "/bin/bat",
        ])
    })
    .as_ref()
}

pub(super) fn resolve_audited_local_binary(candidates: &[&str]) -> Option<PathBuf> {
    resolve_audited_package_binary(candidates, "bat")
}

fn resolve_audited_package_binary(candidates: &[&str], package: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let effective_user = rustix::process::geteuid().as_raw();
        for candidate in candidates {
            let Ok(canonical) = Path::new(candidate).canonicalize() else {
                continue;
            };
            let trusted_prefix = canonical.starts_with("/usr/bin")
                || canonical.starts_with("/bin")
                || canonical.starts_with(format!("/opt/homebrew/Cellar/{package}/"))
                || canonical.starts_with(format!("/usr/local/Cellar/{package}/"));
            let Ok(metadata) = canonical.metadata() else {
                continue;
            };
            if trusted_prefix
                && metadata.is_file()
                && (metadata.uid() == 0 || metadata.uid() == effective_user)
                && metadata.mode() & 0o022 == 0
                && metadata.mode() & 0o111 != 0
            {
                return Some(canonical);
            }
        }
    }
    None
}

pub(super) fn resolve_audited_system_binary(candidates: &[&str]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        for candidate in candidates {
            let Ok(canonical) = Path::new(candidate).canonicalize() else {
                continue;
            };
            if !canonical.starts_with("/usr/bin") && !canonical.starts_with("/bin") {
                continue;
            }
            let Ok(metadata) = canonical.metadata() else {
                continue;
            };
            if metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0 {
                return Some(canonical);
            }
        }
    }
    None
}

pub(crate) fn audited_system_git() -> Option<&'static PathBuf> {
    static SYSTEM_GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    SYSTEM_GIT.get_or_init(resolve_audited_system_git).as_ref()
}

pub(super) fn resolve_audited_system_git() -> Option<PathBuf> {
    resolve_audited_system_binary(&["/usr/bin/git", "/bin/git"])
}
