#!/usr/bin/env python3
"""Keep production networking behind the guarded HTTP and sandbox-proxy boundaries."""

from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parent.parent
HTTP_BOUNDARY = ROOT / "crates/rw-providers/src/http.rs"
# These raw-socket exceptions are the sandbox implementation itself: the
# supervised egress proxy, its Linux loopback wakeup, and a fixture whose sole
# purpose is proving that an MCP child cannot open an outbound connection.
SANDBOX_PROXY_BOUNDARY = ROOT / "crates/rw-sandbox/src/proxy.rs"
SANDBOX_OS_BOUNDARY = ROOT / "crates/rw-sandbox/src/lib.rs"
MCP_SANDBOX_FIXTURE = ROOT / "crates/rw-mcp/src/bin/rw-mcp-fixture.rs"
RAW_NETWORK_BOUNDARIES = {
    HTTP_BOUNDARY,
    SANDBOX_PROXY_BOUNDARY,
    SANDBOX_OS_BOUNDARY,
    MCP_SANDBOX_FIXTURE,
}
FORBIDDEN_DIRECT = (
    (
        "reqwest Client constructor",
        re.compile(r"\breqwest\s*::\s*Client\s*::\s*(?:new|builder|default)\s*\("),
    ),
    ("reqwest::get", re.compile(r"\breqwest\s*::\s*get\s*\(")),
    (
        "reqwest Client import",
        re.compile(
            r"\buse\s+reqwest\s*::\s*(?:Client\b|\{[^}]*\bClient\b)|"
            r"\buse\s+reqwest\s*(?:as\s+[A-Za-z_][A-Za-z0-9_]*)?\s*;"
        ),
    ),
    (
        "TcpStream connection",
        re.compile(r"\bTcpStream\s*::\s*(?:connect|connect_timeout)\s*\("),
    ),
    ("UdpSocket::connect", re.compile(r"\bUdpSocket\s*::\s*connect\s*\(")),
    ("hyper client", re.compile(r"\bhyper\s*::\s*client\b")),
    ("hyper-util client", re.compile(r"\bhyper_util\s*::\s*client\b")),
)


CFG_TEST = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*(?:test|all\s*\(\s*test\b[^]\n]*\))\s*\)\s*\]"
)
CHAR_LITERAL = re.compile(
    r"'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|.)|[^\\'\r\n])'"
)


def _masked_rust(source: str) -> str:
    """Mask comments and literals while preserving byte offsets and newlines."""
    masked = list(source)
    index = 0
    block_depth = 0
    state = "code"
    raw_hashes = 0
    while index < len(source):
        current = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if state == "code":
            if current == "/" and following == "/":
                masked[index] = masked[index + 1] = " "
                index += 2
                state = "line_comment"
                continue
            if current == "/" and following == "*":
                masked[index] = masked[index + 1] = " "
                index += 2
                block_depth = 1
                state = "block_comment"
                continue
            if current == '"':
                masked[index] = " "
                index += 1
                state = "string"
                continue
            if current == "'" and CHAR_LITERAL.match(source, index):
                # Match an actual Rust character literal rather than searching
                # ahead for another apostrophe, which can join two lifetimes and
                # accidentally hide the code between them.
                masked[index] = " "
                index += 1
                state = "char"
                continue
            if current == "r":
                cursor = index + 1
                while cursor < len(source) and source[cursor] == "#":
                    cursor += 1
                if cursor < len(source) and source[cursor] == '"':
                    raw_hashes = cursor - index - 1
                    for offset in range(index, cursor + 1):
                        masked[offset] = " "
                    index = cursor + 1
                    state = "raw_string"
                    continue
            index += 1
            continue
        if state == "line_comment":
            if current == "\n":
                state = "code"
            else:
                masked[index] = " "
            index += 1
            continue
        if state == "block_comment":
            if current == "/" and following == "*":
                masked[index] = masked[index + 1] = " "
                block_depth += 1
                index += 2
            elif current == "*" and following == "/":
                masked[index] = masked[index + 1] = " "
                block_depth -= 1
                index += 2
                if block_depth == 0:
                    state = "code"
            else:
                if current != "\n":
                    masked[index] = " "
                index += 1
            continue
        if state in {"string", "char"}:
            if current == "\\":
                masked[index] = " "
                if index + 1 < len(source):
                    if source[index + 1] != "\n":
                        masked[index + 1] = " "
                    index += 2
                else:
                    index += 1
            else:
                if current != "\n":
                    masked[index] = " "
                index += 1
                if (state == "string" and current == '"') or (
                    state == "char" and current == "'"
                ):
                    state = "code"
            continue
        terminator = '"' + ("#" * raw_hashes)
        if source.startswith(terminator, index):
            for offset in range(index, index + len(terminator)):
                masked[offset] = " "
            index += len(terminator)
            state = "code"
        else:
            if current != "\n":
                masked[index] = " "
            index += 1
    return "".join(masked)


def _cfg_test_item_end(masked: str, start: int) -> int:
    """Return the end offset of one cfg(test)-annotated Rust item."""
    cursor = CFG_TEST.match(masked, start).end()
    while True:
        whitespace = re.match(r"\s*", masked[cursor:])
        cursor += whitespace.end()
        if not masked.startswith("#[", cursor):
            break
        attribute_end = masked.find("]", cursor + 2)
        if attribute_end == -1:
            return len(masked)
        cursor = attribute_end + 1

    semicolon = masked.find(";", cursor)
    opening = masked.find("{", cursor)
    if semicolon != -1 and (opening == -1 or semicolon < opening):
        return semicolon + 1
    if opening == -1:
        return len(masked)

    depth = 0
    for offset in range(opening, len(masked)):
        if masked[offset] == "{":
            depth += 1
        elif masked[offset] == "}":
            depth -= 1
            if depth == 0:
                return offset + 1
    return len(masked)


def production_source(path: Path) -> str:
    source = path.read_text(encoding="utf-8")
    masked = _masked_rust(source)
    retained: list[str] = []
    cursor = 0
    for match in CFG_TEST.finditer(masked):
        if match.start() < cursor:
            continue
        retained.append(masked[cursor : match.start()])
        cursor = _cfg_test_item_end(masked, match.start())
    retained.append(masked[cursor:])
    return "".join(retained)


def _dependency_names_for_reqwest(table: object) -> set[str]:
    if not isinstance(table, dict):
        return set()
    return {
        name
        for name, specification in table.items()
        if name == "reqwest"
        or (isinstance(specification, dict) and specification.get("package") == "reqwest")
    }


def _has_production_reqwest_dependency(
    manifest: Path, workspace_names: set[str] | None = None
) -> bool:
    """Return whether a crate directly uses reqwest in a production target."""
    document = tomllib.loads(manifest.read_text(encoding="utf-8"))
    names = {"reqwest", *(workspace_names or set())}
    dependencies = document.get("dependencies", {})
    if names.intersection(dependencies) or _dependency_names_for_reqwest(dependencies):
        return True
    for target in document.get("target", {}).values():
        if isinstance(target, dict):
            dependencies = target.get("dependencies", {})
            if names.intersection(dependencies) or _dependency_names_for_reqwest(dependencies):
                return True
    return False


def _forbidden_direct_network(source: str) -> list[str]:
    return [label for label, pattern in FORBIDDEN_DIRECT if pattern.search(source)]


def main() -> int:
    failures: list[str] = []
    for path in sorted((ROOT / "crates").glob("*/src/**/*.rs")):
        source = production_source(path)
        if path not in RAW_NETWORK_BOUNDARIES:
            for boundary in _forbidden_direct_network(source):
                failures.append(
                    f"{path.relative_to(ROOT)} bypasses the guarded network boundary with {boundary}"
                )
        if "reqwest::" in source and "crates/rw-providers/src" not in path.as_posix():
            failures.append(
                f"{path.relative_to(ROOT)} imports the private HTTP implementation outside rw-providers"
            )
        if (
            ".send()" in source
            and path != HTTP_BOUNDARY
            and "crates/rw-providers/src" in path.as_posix()
        ):
            guarded = (
                "build_client_with_proxy_auth" in source
                or "require_process_network" in source
            )
            if not guarded:
                failures.append(
                    f"{path.relative_to(ROOT)} sends HTTP without the shared builder or process guard"
                )

    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    workspace_names = _dependency_names_for_reqwest(
        workspace.get("workspace", {}).get("dependencies", {})
    )
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        if manifest.parent.name == "rw-providers":
            continue
        if _has_production_reqwest_dependency(manifest, workspace_names):
            failures.append(
                f"{manifest.relative_to(ROOT)} depends directly on the private HTTP client"
            )

    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("production network boundaries: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
