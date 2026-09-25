//! Explicit search grammars: unknown options remain approval-required.
use std::{path::PathBuf, sync::OnceLock};

pub(in crate::bash) fn audited_rg() -> Option<&'static PathBuf> {
    static RG: OnceLock<Option<PathBuf>> = OnceLock::new();
    RG.get_or_init(|| {
        super::resolve_audited_package_binary(
            &[
                "/opt/homebrew/bin/rg",
                "/usr/local/bin/rg",
                "/usr/bin/rg",
                "/bin/rg",
            ],
            "ripgrep",
        )
    })
    .as_ref()
}

pub(in crate::bash) fn safe_search_arguments(name: &str, arguments: &[String]) -> bool {
    if name == "find" {
        return safe_find(arguments);
    }
    let mut index = 0;
    let mut positional = false;
    while let Some(argument) = arguments.get(index) {
        index += 1;
        if positional || !argument.starts_with('-') || argument == "-" {
            continue;
        }
        if argument == "--" {
            positional = true;
            continue;
        }
        let (option, value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(a, b)| (a, Some(b)));
        let value_option = takes_value(name, option);
        if value_option {
            if value.is_none() {
                if arguments.get(index).is_none() {
                    return false;
                }
                index += 1;
            }
            continue;
        }
        if value.is_some() {
            return false;
        }
        let flag = safe_flag(name, option);
        if flag {
            continue;
        }
        if option.starts_with("--") {
            return false;
        }
        for (offset, character) in option[1..].char_indices() {
            if "efmABC".contains(character) || (name == "rg" && "gtTj".contains(character)) {
                if offset + character.len_utf8() == option.len() - 1 {
                    if arguments.get(index).is_none() {
                        return false;
                    }
                    index += 1;
                }
                break;
            }
            if !("ivnclLFwxoqsHh".contains(character)
                || (name == "grep" && "rREIG".contains(character))
                || (name == "rg" && "SNUa0".contains(character)))
            {
                return false;
            }
        }
    }
    true
}

fn safe_find(arguments: &[String]) -> bool {
    let mut index = 0;
    let mut expression = false;
    while let Some(argument) = arguments.get(index) {
        index += 1;
        if !expression
            && !argument.starts_with('-')
            && !matches!(argument.as_str(), "!" | "(" | ")")
        {
            continue;
        }
        expression = true;
        match argument.as_str() {
            "-name" | "-iname" | "-path" | "-ipath" | "-type" | "-maxdepth" | "-mindepth"
            | "-size" | "-mtime" | "-mmin" => {
                if arguments.get(index).is_none() {
                    return false;
                }
                index += 1;
            }
            "-print" | "-print0" | "-empty" | "-prune" | "-depth" | "-xdev" | "-a" | "-and"
            | "-o" | "-or" | "-not" | "!" | "(" | ")" => {}
            _ => return false,
        }
    }
    true
}

fn takes_value(name: &str, option: &str) -> bool {
    match name {
        "grep" => matches!(
            option,
            "--regexp"
                | "--file"
                | "--include"
                | "--exclude"
                | "--exclude-dir"
                | "--max-count"
                | "--after-context"
                | "--before-context"
                | "--context"
        ),
        "rg" => matches!(
            option,
            "--regexp"
                | "--file"
                | "--glob"
                | "--iglob"
                | "--type"
                | "--type-not"
                | "--max-count"
                | "--max-depth"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "--color"
                | "--threads"
        ),
        _ => false,
    }
}
fn safe_flag(name: &str, option: &str) -> bool {
    matches!(
        option,
        "--ignore-case"
            | "--invert-match"
            | "--line-number"
            | "--count"
            | "--files-with-matches"
            | "--files-without-match"
            | "--fixed-strings"
            | "--word-regexp"
            | "--line-regexp"
            | "--only-matching"
            | "--quiet"
            | "--no-messages"
    ) || (name == "grep"
        && matches!(
            option,
            "--recursive" | "--extended-regexp" | "--basic-regexp"
        ))
        || (name == "rg"
            && matches!(
                option,
                "--files"
                    | "--hidden"
                    | "--no-ignore"
                    | "--no-config"
                    | "--json"
                    | "--no-heading"
                    | "--heading"
                    | "--smart-case"
                    | "--null"
                    | "--text"
            ))
}
