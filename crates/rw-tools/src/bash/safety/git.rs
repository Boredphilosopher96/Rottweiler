//! Read-only git grammars for the built-in safe-list.
//!
//! A subcommand is accepted only in forms that read repository state. Forms
//! that write refs, the index, config or files, start a pager, editor, help
//! viewer, external diff, textconv filter or signature program, or contact a
//! remote remain approval-required. Execution adds fixed `-c` overrides and
//! subcommand flags (see `hardened_git_argv`) so repository configuration
//! cannot select those programs either.

/// Returns whether `git <subcommand> <arguments>` is a recognized read-only form.
pub(in crate::bash) fn safe_git_arguments(subcommand: &str, arguments: &[String]) -> bool {
    if arguments.iter().any(|argument| argument == "--help") {
        return false;
    }
    match subcommand {
        "status" => safe_status(arguments),
        "diff" => safe_revision_output(arguments) && !has(arguments, "--no-index"),
        "log" | "show" | "shortlog" => safe_revision_output(arguments),
        "reflog" => safe_reflog(arguments),
        "blame" => !has(arguments, "--textconv"),
        "grep" => safe_grep(arguments),
        "branch" => safe_branch(arguments),
        "rev-parse" => !arguments
            .iter()
            .any(|argument| argument == "--parseopt" || argument.starts_with("--stdin")),
        "ls-files" => true,
        "remote" => safe_remote(arguments),
        "describe" => safe_describe(arguments),
        "config" => safe_config(arguments),
        _ => false,
    }
}

/// The argv that follows the audited executable and global hardening flags.
/// Output-producing subcommands always disable external diff and textconv;
/// `reflog` is normalized to its read-only `show` form.
pub(in crate::bash) fn hardened_git_subcommand(
    subcommand: &str,
    mut arguments: Vec<String>,
) -> Vec<String> {
    let (subcommand, fixed): (&[&str], &[&str]) = match subcommand {
        "diff" => (&["diff"], &["--no-ext-diff", "--no-textconv"]),
        "log" => (&["log"], &["--no-ext-diff", "--no-textconv"]),
        "show" => (&["show"], &["--no-ext-diff", "--no-textconv"]),
        "reflog" => {
            if arguments.first().is_some_and(|argument| argument == "show") {
                arguments.remove(0);
            }
            (&["reflog", "show"], &["--no-ext-diff", "--no-textconv"])
        }
        "blame" => (&["blame"], &["--no-textconv"]),
        "grep" => (&["grep"], &["--no-textconv"]),
        other => {
            let mut argv = vec![other.to_owned()];
            argv.extend(arguments);
            return argv;
        }
    };
    subcommand
        .iter()
        .chain(fixed)
        .map(|argument| (*argument).to_owned())
        .chain(arguments)
        .collect()
}

fn has(arguments: &[String], option: &str) -> bool {
    arguments.iter().any(|argument| argument == option)
}

fn safe_status(arguments: &[String]) -> bool {
    let mut pathspecs = false;
    for argument in arguments {
        if pathspecs {
            continue;
        }
        if argument == "--" {
            pathspecs = true;
            continue;
        }
        if !matches!(
            argument.as_str(),
            "--short"
                | "-s"
                | "--branch"
                | "-b"
                | "--show-stash"
                | "--porcelain"
                | "--porcelain=v1"
                | "--porcelain=v2"
                | "--untracked-files=no"
                | "--untracked-files=normal"
                | "--untracked-files=all"
                | "-uno"
                | "-unormal"
                | "-uall"
                | "--ignored=no"
                | "--ignored=matching"
                | "--ignored=traditional"
                | "--renames"
                | "--no-renames"
                | "--ahead-behind"
                | "--no-ahead-behind"
        ) {
            return false;
        }
    }
    true
}

/// Log/diff machinery: reject file output, external programs and signature
/// verification (`--show-signature` and `%G` format placeholders run gpg).
fn safe_revision_output(arguments: &[String]) -> bool {
    !arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "--output" | "--ext-diff" | "--textconv" | "--show-signature"
        ) || argument.starts_with("--output=")
            || argument.contains("%G")
    })
}

/// `git reflog` and `git reflog show ...`; every other reflog subcommand writes.
fn safe_reflog(arguments: &[String]) -> bool {
    let options = match arguments.first().map(String::as_str) {
        None => return true,
        Some("show") => &arguments[1..],
        Some(first) if first.starts_with('-') => arguments,
        Some(_) => return false,
    };
    safe_revision_output(options)
}

/// `-O`/`--open-files-in-pager` runs a program; so does textconv.
fn safe_grep(arguments: &[String]) -> bool {
    !arguments.iter().any(|argument| {
        argument == "--textconv"
            || argument == "--ext-grep"
            || argument.starts_with("--open-files-in-pager")
            || (argument.starts_with('-') && !argument.starts_with("--") && argument.contains('O'))
    })
}

/// Listing forms only. A positional name creates a branch unless list mode is
/// explicit (`--list`) or implied by a ref filter.
fn safe_branch(arguments: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "-a",
        "--all",
        "-r",
        "--remotes",
        "-v",
        "-vv",
        "--verbose",
        "-l",
        "--list",
        "--show-current",
        "-i",
        "--ignore-case",
        "--omit-empty",
        "--color",
        "--no-color",
        "--no-column",
        "--abbrev",
        "--no-abbrev",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
    ];
    const VALUES: &[&str] = &["--sort", "--format", "--points-at"];
    const PREFIXES: &[&str] = &[
        "--color=",
        "--abbrev=",
        "--contains=",
        "--no-contains=",
        "--merged=",
        "--no-merged=",
    ];
    const LIST_MODE: &[&str] = &[
        "-l",
        "--list",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
    ];
    // Short listing flags may be clustered (`-av`, `-rl`).
    let cluster = |argument: &str| {
        argument.len() > 2
            && argument.starts_with('-')
            && !argument.starts_with("--")
            && argument[1..].chars().all(|flag| "arvli".contains(flag))
    };
    let expanded = arguments
        .iter()
        .flat_map(|argument| {
            if cluster(argument) {
                argument[1..]
                    .chars()
                    .map(|flag| format!("-{flag}"))
                    .collect()
            } else {
                vec![argument.clone()]
            }
        })
        .collect::<Vec<_>>();
    if expanded
        .iter()
        .any(|argument| argument.contains("%(signature"))
    {
        return false;
    }
    let Some(positionals) = options(&expanded, FLAGS, VALUES, PREFIXES) else {
        return false;
    };
    positionals == 0
        || expanded.iter().any(|argument| {
            let option = argument
                .split_once('=')
                .map_or(argument.as_str(), |(o, _)| o);
            LIST_MODE.contains(&option)
        })
}

/// `git remote [-v]` and `git remote get-url [--push] [--all] <name>`.
fn safe_remote(arguments: &[String]) -> bool {
    if arguments
        .first()
        .is_some_and(|argument| argument == "get-url")
    {
        return options(&arguments[1..], &["--push", "--all"], &[], &[]) == Some(1);
    }
    arguments
        .iter()
        .all(|argument| matches!(argument.as_str(), "-v" | "--verbose"))
}

/// `--dirty` and `--broken` refresh the index.
fn safe_describe(arguments: &[String]) -> bool {
    options(
        arguments,
        &[
            "--all",
            "--tags",
            "--contains",
            "--long",
            "--always",
            "--exact-match",
            "--first-parent",
            "--debug",
        ],
        &["--match", "--exclude"],
        &["--abbrev=", "--candidates="],
    )
    .is_some()
}

/// `git config get|list ...` and the `--get*`/`--list` action forms. A
/// positional without an explicit read action would set a value.
fn safe_config(arguments: &[String]) -> bool {
    const SCOPE: &[&str] = &[
        "--local",
        "--global",
        "--system",
        "--worktree",
        "--includes",
        "--no-includes",
        "--show-origin",
        "--show-scope",
        "--name-only",
        "-z",
        "--null",
        "--bool",
        "--int",
        "--bool-or-int",
        "--path",
        "--expiry-date",
    ];
    const VALUES: &[&str] = &["-f", "--file", "--blob", "--type", "--default"];
    const GET_FLAGS: &[&str] = &["--all", "--regexp", "--fixed-value", "--show-names"];
    const GET_VALUES: &[&str] = &["--value", "--url"];
    let flags = |extra: &[&'static str]| {
        SCOPE
            .iter()
            .chain(extra)
            .copied()
            .collect::<Vec<&'static str>>()
    };
    let values = |extra: &[&'static str]| {
        VALUES
            .iter()
            .chain(extra)
            .copied()
            .collect::<Vec<&'static str>>()
    };
    match arguments.first().map(String::as_str) {
        Some("get") => {
            return options(&arguments[1..], &flags(GET_FLAGS), &values(GET_VALUES), &[])
                == Some(1);
        }
        Some("list") => return options(&arguments[1..], SCOPE, VALUES, &[]) == Some(0),
        _ => {}
    }
    let actions = [
        ("--get", 1..=2),
        ("--get-all", 1..=2),
        ("--get-regexp", 1..=2),
        ("--get-urlmatch", 2..=2),
        ("--list", 0..=0),
        ("-l", 0..=0),
    ];
    let chosen = actions
        .iter()
        .filter(|(action, _)| has(arguments, action))
        .collect::<Vec<_>>();
    let [(_, positionals)] = chosen.as_slice() else {
        return false;
    };
    let action_flags = actions
        .iter()
        .map(|(action, _)| *action)
        .collect::<Vec<_>>();
    options(arguments, &flags(&action_flags), VALUES, &[])
        .is_some_and(|count| positionals.contains(&count))
}

/// Parses an explicit option grammar and returns the positional count, or
/// `None` when an argument is not part of the grammar. Value options accept
/// `--option=value` or a separate value; arguments after `--` are positional.
fn options(
    arguments: &[String],
    flags: &[&str],
    values: &[&str],
    prefixes: &[&str],
) -> Option<usize> {
    let mut positionals = 0;
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        index += 1;
        if argument == "--" {
            return Some(positionals + arguments.len() - index);
        }
        if !argument.starts_with('-') || argument == "-" {
            positionals += 1;
            continue;
        }
        if flags.contains(&argument.as_str())
            || prefixes.iter().any(|prefix| argument.starts_with(prefix))
        {
            continue;
        }
        if values.contains(&argument.as_str()) {
            arguments.get(index)?;
            index += 1;
            continue;
        }
        let (option, _) = argument.split_once('=')?;
        if !values.contains(&option) {
            return None;
        }
    }
    Some(positionals)
}
