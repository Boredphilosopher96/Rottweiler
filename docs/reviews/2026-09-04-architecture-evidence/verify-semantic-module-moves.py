"""Compare moved Rust function bodies against the pre-split checkpoint.

Requires tree-sitter==0.25.2 and tree-sitter-rust==0.24.2. Run from the repository:
  python3 docs/reviews/2026-09-04-architecture-evidence/verify-semantic-module-moves.py
This is evidence for the split checkpoint, not a permanent no-change policy.
"""
from collections import Counter
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

from tree_sitter import Language, Parser
import tree_sitter_rust

ROOTS = (
    "crates/rw-ext/src/discovery.rs",
    "crates/rw-ext/src/plugin_runtime.rs",
    "crates/rw-runtime/src/extension_config.rs",
    "crates/rw-runtime/src/extension_runtime.rs",
    "crates/rw-core/src/provider_factory.rs",
    "crates/rw-core/src/mcp.rs",
    "crates/rw-providers/src/auth.rs",
    "crates/rw-providers/src/openai.rs",
    "crates/rw-providers/src/recording.rs",
    "crates/rw-core/tests/provider_factory.rs",
)
# Rustfmt removes a redundant closure block in this moved test. Both exact
# body hashes are retained so this exception cannot hide a later test change.
FORMATTED_TEST = "repeated_requests_record_and_replay_distinct_occurrences_in_order"
BEFORE_FORMAT = "f06144d46cf110cd033e5b3151d64e6054929bde74f69bc08b60b69af17a63dd"
AFTER_FORMAT = "de570027c11b13032be3e76f27dc7c5866bd3b7e9dfd1640aaf719f1db10ccb0"


def git(*args):
    return subprocess.check_output(["git", *args])


def bodies(parser, data):
    tree = parser.parse(data)
    assert not tree.root_node.has_error, "invalid Rust source"
    found = Counter()

    def tokens(node):
        if node.type in ("line_comment", "block_comment", ","):
            return []
        if not node.children:
            return [data[node.start_byte:node.end_byte]]
        return [token for child in node.children for token in tokens(child)]

    def visit(node):
        if node.type == "function_item":
            body = node.child_by_field_name("body")
            if body:
                name = node.child_by_field_name("name").text.decode()
                digest = hashlib.sha256(b"\0".join(tokens(body))).hexdigest()
                found[(name, digest)] += 1
        for child in node.named_children:
            visit(child)

    visit(tree.root_node)
    return found


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--before", default="5baf21a")
    cli.add_argument("--after", default="WORKTREE")
    cli.add_argument("--roots", nargs="+", default=ROOTS)
    args = cli.parse_args()
    parser = Parser(Language(tree_sitter_rust.language()))
    old_files = set(git("ls-tree", "-r", "--name-only", args.before).decode().splitlines())
    if args.after == "WORKTREE":
        new_files = set(git("ls-files", "--cached", "--others", "--exclude-standard").decode().splitlines())
        read_after = lambda path: Path(path).read_bytes()
    else:
        new_files = set(git("ls-tree", "-r", "--name-only", args.after).decode().splitlines())
        read_after = lambda path: git("show", args.after + ":" + path)
    for path in args.roots:
        before = bodies(parser, git("show", args.before + ":" + path))
        prefix = str(Path(path).with_suffix("")) + "/"
        moved = [path, *sorted(p for p in new_files - old_files if p.startswith(prefix) and p.endswith(".rs"))]
        after = Counter()
        for item in moved:
            after.update(bodies(parser, read_after(item)))
        lost, added = before - after, after - before
        if path == "crates/rw-providers/src/recording.rs":
            assert lost == Counter({(FORMATTED_TEST, BEFORE_FORMAT): 1})
            assert added == Counter({(FORMATTED_TEST, AFTER_FORMAT): 1})
        else:
            assert not lost and not added, (path, lost, added)
        maximum = max(len(read_after(item).splitlines()) for item in moved)
        assert maximum <= 1500, (path, maximum)
        print(json.dumps({"file": path, "functions_preserved": sum(before.values()), "maximum_file_lines": maximum}))


if __name__ == "__main__":
    main()
