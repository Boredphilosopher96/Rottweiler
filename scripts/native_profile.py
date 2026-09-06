"""Owned code-generation settings for native release products and their receipts."""
from __future__ import annotations

import hashlib
import os
import sys
from pathlib import Path

UNWIND_SCRIPT = "scripts/native-linux-unwind.ld"
UNWIND_SCRIPT_TOKEN = "@RW_NATIVE_UNWIND_SCRIPT@"


def settings(target: str, repo: Path | None = None) -> dict:
    if target.endswith("-apple-darwin"):
        return {"opt_level": "3", "rustflags": []}
    if target.endswith(("-linux-gnu", "-linux-musl")):
        flags = ["-C", "force-unwind-tables=no"]
        if target.endswith("-linux-gnu"):
            # DT_RELR requires glibc 2.36. Official GNU artifacts already use a
            # newer loader ABI; musl has a separate, unqualified loader floor.
            flags += ["-C", "link-arg=-Wl,-z,pack-relative-relocs"]
            # Precompiled std and native inputs can retain unwind sections even
            # with force-unwind-tables=no. Own their final-link removal too.
            final_flags = ["-C", "panic=abort", "-C", "link-arg=-Wl,--no-eh-frame-hdr", "-C", f"link-arg=-T{UNWIND_SCRIPT_TOKEN}"]
            root = repo or Path(__file__).resolve().parent.parent
            script = {"path": UNWIND_SCRIPT, "sha256": hashlib.sha256((root / UNWIND_SCRIPT).read_bytes()).hexdigest()}
            return {"opt_level": "s", "panic": "abort", "rustflags": flags, "final_rustflags": final_flags, "linker_script": script}
        return {"opt_level": "s", "rustflags": flags}
    raise ValueError(f"unsupported native release target: {target}")


def verification_settings(target: str) -> dict:
    """Optimized test/instrumentation harnesses retain native unwinding.

    Cargo libtests use panic=unwind even when the product release profile aborts.
    Their process loads the receipt-bound product helper for artifact validation.
    """
    profile = settings(target)
    return {"opt_level": profile["opt_level"], "rustflags": profile["rustflags"]}


def final_rustflags(target: str) -> list[str]:
    profile = settings(target)
    script = str(Path(__file__).resolve().parent.parent / UNWIND_SCRIPT)
    return [flag.replace(UNWIND_SCRIPT_TOKEN, script) for flag in profile.get("final_rustflags", [])]


def product_command(target: str, arguments: list[str]) -> list[str]:
    if not arguments or arguments[0] != "build" or "--bin" not in arguments or any(
        argument in {"--tests", "--test", "--all-targets", "--"} or argument.startswith("--test=")
        for argument in arguments[1:]
    ):
        raise ValueError("native product build requires one explicit --bin and no test targets")
    final_flags = final_rustflags(target)
    if final_flags:
        return ["cargo", "rustc", *arguments[1:], "--", *final_flags]
    return ["cargo", *arguments]


def verification_environment(target: str, inherited: dict[str, str]) -> dict[str, str]:
    return _environment(verification_settings(target), inherited)


def environment(target: str, inherited: dict[str, str]) -> dict[str, str]:
    return _environment(settings(target), inherited)


def _environment(profile: dict, inherited: dict[str, str]) -> dict[str, str]:
    result = dict(inherited)
    encoded = result.get("CARGO_ENCODED_RUSTFLAGS")
    # Match Cargo's existing precedence and whitespace tokenization. The owned
    # flags come last, so caller defaults cannot undo the native product policy.
    flags = (encoded.split("\x1f") if encoded else []) if encoded is not None else result.get("RUSTFLAGS", "").split()
    if profile.get("panic") == "abort":
        # Cargo's profile controls dependency cfg(panic) too. Explicit compiler
        # overrides must not silently compile Wasmtime's unwind-only branches.
        for index, flag in enumerate(flags):
            option = flag.removeprefix("--codegen=").removeprefix("-C")
            if flag in {"-C", "--codegen"} and index + 1 < len(flags):
                option = flags[index + 1]
            if option.startswith("panic=") and option != "panic=abort":
                raise ValueError("GNU native product dependencies require panic=abort")
        result["CARGO_PROFILE_RELEASE_PANIC"] = "abort"
    flags += profile["rustflags"]
    result["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    result["CARGO_PROFILE_RELEASE_OPT_LEVEL"] = profile["opt_level"]
    result["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    return result


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit("usage: native_profile.py TARGET CARGO_ARGUMENTS...")
    command = product_command(sys.argv[1], sys.argv[2:])
    os.execvpe("cargo", command, environment(sys.argv[1], dict(os.environ)))


if __name__ == "__main__":
    main()
