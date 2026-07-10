#!/usr/bin/env python3
"""Keep production outbound networking behind rw-providers' guarded client."""

from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parent.parent
HTTP_BOUNDARY = ROOT / "crates/rw-providers/src/http.rs"
FORBIDDEN_DIRECT = (
    "reqwest::Client::new(",
    "reqwest::Client::builder(",
    "reqwest::get(",
    "TcpStream::connect(",
    "UdpSocket::connect(",
    "hyper::client",
    "hyper_util::client",
)


def production_source(path: Path) -> str:
    source = path.read_text(encoding="utf-8")
    return source.split("#[cfg(test)]", 1)[0]


def main() -> int:
    failures: list[str] = []
    for path in sorted((ROOT / "crates").glob("*/src/**/*.rs")):
        source = production_source(path)
        if path != HTTP_BOUNDARY:
            for token in FORBIDDEN_DIRECT:
                if token in source:
                    failures.append(
                        f"{path.relative_to(ROOT)} bypasses the guarded network boundary with {token!r}"
                    )
        if ".send()" in source and path != HTTP_BOUNDARY:
            guarded = (
                "build_client_with_proxy_auth" in source
                or "require_process_network" in source
            )
            if not guarded:
                failures.append(
                    f"{path.relative_to(ROOT)} sends HTTP without the shared builder or process guard"
                )

    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("production network boundaries: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
