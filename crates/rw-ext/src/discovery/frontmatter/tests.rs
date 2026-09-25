#![allow(clippy::expect_used)]

use super::{FrontmatterValue, parse_frontmatter};
use std::path::Path;

fn fields(contents: &str) -> std::collections::BTreeMap<String, FrontmatterValue> {
    parse_frontmatter(Path::new("SKILL.md"), contents)
        .expect("frontmatter parses")
        .fields
}

fn scalar(fields: &std::collections::BTreeMap<String, FrontmatterValue>, key: &str) -> String {
    match fields.get(key) {
        Some(FrontmatterValue::Scalar(value)) => value.clone(),
        other => panic!("{key} is not a scalar: {other:?}"),
    }
}

#[test]
fn literal_block_scalars_honor_chomping() {
    let parsed = fields(
        "---\nclip: |\n  one\n  two\n\nstrip: |-\n  one\n\nkeep: |+\n  one\n\n\nlast: done\n---\n",
    );
    assert_eq!(scalar(&parsed, "clip"), "one\ntwo\n");
    assert_eq!(scalar(&parsed, "strip"), "one");
    assert_eq!(scalar(&parsed, "keep"), "one\n\n\n");
    assert_eq!(scalar(&parsed, "last"), "done");
}

#[test]
fn folded_block_scalars_join_lines_and_keep_paragraphs() {
    let parsed = fields(
        "---\nfolded: >\n  first line\n  continues\n\n  second paragraph\n    indented stays\n  back\nstripped: >-\n  a\n  b\n---\n",
    );
    assert_eq!(
        scalar(&parsed, "folded"),
        "first line continues\nsecond paragraph\n  indented stays\nback\n"
    );
    assert_eq!(scalar(&parsed, "stripped"), "a b");
}

#[test]
fn explicit_indentation_indicator_preserves_leading_spaces() {
    let parsed = fields("---\ncode: |2\n    indented\n  flush\n---\n");
    assert_eq!(scalar(&parsed, "code"), "  indented\nflush\n");
}

#[test]
fn block_scalar_keeps_hash_lines_as_content() {
    let parsed = fields("---\ndescription: |\n  # heading\n  text # not a comment\n---\n");
    assert_eq!(
        scalar(&parsed, "description"),
        "# heading\ntext # not a comment\n"
    );
}

#[test]
fn nested_structures_under_unknown_keys_are_opaque() {
    let parsed = fields(
        "---\nname: x\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\nmetadata:\n  - key: value\nmap: {a: 1}\ndescription: kept\n---\n",
    );
    assert_eq!(parsed.get("hooks"), Some(&FrontmatterValue::Nested));
    assert_eq!(parsed.get("metadata"), Some(&FrontmatterValue::Nested));
    assert_eq!(parsed.get("map"), Some(&FrontmatterValue::Nested));
    assert_eq!(scalar(&parsed, "description"), "kept");
}

#[test]
fn sequences_accept_indented_zero_indented_and_inline_forms() {
    let parsed = fields(
        "---\nindented:\n  - Bash\n  - \"Read\"\nflush:\n- Edit # comment\n- 'Write'\ninline: [Grep, \"Glob\"]\n---\n",
    );
    assert_eq!(
        parsed.get("indented"),
        Some(&FrontmatterValue::List(vec!["Bash".into(), "Read".into()]))
    );
    assert_eq!(
        parsed.get("flush"),
        Some(&FrontmatterValue::List(vec!["Edit".into(), "Write".into()]))
    );
    assert_eq!(
        parsed.get("inline"),
        Some(&FrontmatterValue::List(vec!["Grep".into(), "Glob".into()]))
    );
}

#[test]
fn plain_scalars_fold_continuation_lines_and_strip_comments() {
    let parsed = fields(
        "---\ndescription: first\n  second\nmodel: fast # comment\nquoted: \"a: b\" # comment\nurl: http://example.test/#anchor\n---\n",
    );
    assert_eq!(scalar(&parsed, "description"), "first second");
    assert_eq!(scalar(&parsed, "model"), "fast");
    assert_eq!(scalar(&parsed, "quoted"), "a: b");
    assert_eq!(scalar(&parsed, "url"), "http://example.test/#anchor");
}

#[test]
fn malformed_frontmatter_is_still_rejected() {
    for contents in [
        "---\n name: bad\n---\n",
        "---\nname bad\n---\n",
        "---\nname: a\nname: b\n---\n",
        "---\ntools:\n  -\n---\n",
        "---\ndescription: |\n    deep\n  shallow\n---\n",
        "---\ndescription: \"open\n---\n",
        "---\ntools: [a, b\n---\n",
    ] {
        assert!(
            parse_frontmatter(Path::new("SKILL.md"), contents).is_err(),
            "accepted malformed frontmatter: {contents:?}"
        );
    }
}

#[test]
fn real_claude_skill_frontmatter_parses() {
    for (name, contents) in [
        ("review", include_str!("fixtures/review.md")),
        ("careful", include_str!("fixtures/careful.md")),
        ("plan-tune", include_str!("fixtures/plan-tune.md")),
    ] {
        let document = parse_frontmatter(Path::new("SKILL.md"), contents)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(scalar(&document.fields, "name"), name);
        let description = scalar(&document.fields, "description");
        assert!(description.contains("(gstack)"), "{name}: {description}");
        assert!(matches!(
            document.fields.get("allowed-tools"),
            Some(FrontmatterValue::List(tools)) if tools.contains(&"Bash".to_owned())
        ));
        assert_eq!(document.body, "Body placeholder.\n");
    }
    let careful = parse_frontmatter(Path::new("SKILL.md"), include_str!("fixtures/careful.md"))
        .expect("careful");
    assert_eq!(careful.fields.get("hooks"), Some(&FrontmatterValue::Nested));
    let plan_tune = parse_frontmatter(Path::new("SKILL.md"), include_str!("fixtures/plan-tune.md"))
        .expect("plan-tune");
    assert!(scalar(&plan_tune.fields, "description").contains("\n\nUse when asked"));
}
