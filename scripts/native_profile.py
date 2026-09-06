"""Owned code-generation settings for native release products and their receipts."""
from __future__ import annotations

import os
import sys


def settings(target: str) -> dict:
    if target.endswith("-apple-darwin"):
        return {"opt_level": "3", "rustflags": []}
    if target.endswith(("-linux-gnu", "-linux-musl")):
        flags = ["-C", "force-unwind-tables=no"]
        if target.endswith("-linux-gnu"):
            # DT_RELR requires glibc 2.36. Official GNU artifacts already use a
            # newer loader ABI; musl has a separate, unqualified loader floor.
            flags += ["-C", "link-arg=-Wl,-z,pack-relative-relocs"]
        return {"opt_level": "s", "rustflags": flags}
    raise ValueError(f"unsupported native release target: {target}")


def environment(target: str, inherited: dict[str, str]) -> dict[str, str]:
    profile = settings(target)
    result = dict(inherited)
    encoded = result.get("CARGO_ENCODED_RUSTFLAGS")
    # Match Cargo's existing precedence and whitespace tokenization. The owned
    # flags come last, so caller defaults cannot undo the native product policy.
    flags = (encoded.split("\x1f") if encoded else []) if encoded is not None else result.get("RUSTFLAGS", "").split()
    flags += profile["rustflags"]
    result["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    result["CARGO_PROFILE_RELEASE_OPT_LEVEL"] = profile["opt_level"]
    result["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    return result


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit("usage: native_profile.py TARGET CARGO_ARGUMENTS...")
    os.execvpe("cargo", ["cargo", *sys.argv[2:]], environment(sys.argv[1], dict(os.environ)))


if __name__ == "__main__":
    main()
