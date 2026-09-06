"""Identify and verify native products consumed by build and acceptance gates."""
from __future__ import annotations

import hashlib
import argparse
import json
import os
from pathlib import Path
import platform as host_platform
import subprocess
import tarfile
import tomllib

import artifact_bundle
from release_contract import load_contract, validate_build, verify_archive

RECEIPT = "build.json"
MAX_RECEIPT_BYTES = 128 * 1024


def output(command: list[str], repo: Path) -> str:
    return subprocess.check_output(command, cwd=repo, text=True).strip()


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def source_identity(repo: Path) -> dict[str, str]:
    """Include dirty and untracked source, while Git excludes build output."""
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo
    ).split(b"\0")
    digest = hashlib.sha256()
    for raw in sorted(set(names) - {b""}):
        name = os.fsdecode(raw)
        path = repo / name
        digest.update(raw + b"\0")
        if path.is_symlink():
            digest.update(b"link\0" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            digest.update(b"file\0" + str(path.stat().st_mode & 0o777).encode() + b"\0")
            digest.update(hash_file(path).encode())
        elif not path.exists():
            digest.update(b"deleted")
        else:
            raise ValueError(f"unsupported source entry: {name}")
        digest.update(b"\0")
    return {"commit": output(["git", "rev-parse", "HEAD"], repo), "tree_sha256": digest.hexdigest()}


def pinned_toolchains(repo: Path) -> tuple[str, str]:
    rust = tomllib.loads((repo / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    managers = {
        json.loads((repo / "packages" / name / "package.json").read_text())["packageManager"]
        for name in ("tui", "plugin-sdk", "plugin-host", "js-host")
    }
    if len(managers) != 1 or not next(iter(managers)).startswith("bun@"):
        raise ValueError("native packages must declare one exact Bun version")
    bun = managers.pop().removeprefix("bun@")
    if bun != (repo / ".bun-version").read_text().strip():
        raise ValueError("package-manager pins differ from .bun-version")
    return rust, bun


def configuration_fingerprints(repo: Path) -> dict[str, str]:
    candidates = [Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))]
    candidates += [parent / ".cargo" for parent in (repo, *repo.parents)]
    result = {}
    for index, directory in enumerate(candidates):
        for name in ("config", "config.toml"):
            path = directory / name
            if path.is_file():
                # Hash configuration; never publish credential-bearing configuration text.
                result[f"cargo-config-{index}-{name}"] = hash_file(path)
    return result


def build_identity(repo: Path) -> dict:
    contract = load_contract(repo / "contracts/release-contract.json")
    platform = contract.resolve_platform(host_platform.system(), host_platform.machine())
    rust, bun = pinned_toolchains(repo)
    rust_identity = output(["rustc", "-vV"], repo)
    bun_identity = output(["bun", "--revision"], repo)
    if not rust_identity.startswith(f"rustc {rust} "):
        raise ValueError("candidate compiler differs from rust-toolchain.toml")
    if output(["bun", "--version"], repo) != bun:
        raise ValueError("candidate Bun differs from the package-manager pin")
    target = next(line.removeprefix("host: ") for line in rust_identity.splitlines() if line.startswith("host: "))
    version = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    environment = {
        name: hashlib.sha256(value.encode()).hexdigest() for name, value in sorted(os.environ.items())
        if name in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CC", "CXX", "AR", "SOURCE_DATE_EPOCH", "OPENTUI_LIBC", "ROTTWEILER_STRIP_BIN",
                    "ROTTWEILER_SDK_TEST_FAIL_AFTER_STAGE", "ROTTWEILER_UPDATE_ROOT_KEYS_JSON",
                    "ROTTWEILER_UPDATE_ROOT_THRESHOLD", "ROTTWEILER_UPDATE_ROOT_VERSION", "ROTTWEILER_UPDATE_BASE_URL"}
        or name.startswith(("CARGO_PROFILE_RELEASE_", "CARGO_TARGET_"))
    }
    return {
        "source": source_identity(repo),
        "platform": platform.id,
        "target": target,
        "version": version,
        "toolchains": {"rust": rust_identity, "bun": bun_identity},
        "profile": {"name": "release", "debug": 0,
                    "opt_level": "3" if platform.system == "Darwin" else "s",
                    "environment": environment},
        "cargo_configuration": configuration_fingerprints(repo),
    }


def identity_key(identity: dict) -> str:
    return hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def read_receipt(root: Path) -> dict:
    path = root / RECEIPT
    if root.is_symlink() or path.is_symlink() or not path.is_file():
        raise ValueError("candidate build receipt must be a regular file")
    with path.open("rb") as stream:
        raw = stream.read(MAX_RECEIPT_BYTES + 1)
    if len(raw) > MAX_RECEIPT_BYTES:
        raise ValueError("candidate build receipt exceeds its byte limit")
    receipt = json.loads(raw)
    if not isinstance(receipt, dict) or set(receipt) != {"schema_version", "identity", "identity_sha256", "origin", "components"}:
        raise ValueError("invalid candidate build receipt")
    if receipt["schema_version"] != 1 or not isinstance(receipt["identity"], dict):
        raise ValueError("unsupported candidate build receipt")
    identity = receipt["identity"]
    if set(identity) != {"source", "platform", "target", "version", "toolchains", "profile", "cargo_configuration"}:
        raise ValueError("invalid candidate build identity")
    if receipt["identity_sha256"] != identity_key(identity):
        raise ValueError("candidate build identity checksum differs")
    return receipt


def verify(root: Path, repo: Path, *, expected_identity: dict | None = None) -> dict:
    root = root.absolute()
    receipt = read_receipt(root)
    identity = receipt["identity"]
    contract = load_contract(repo / "contracts/release-contract.json")
    platform = contract.resolve_platform(host_platform.system(), host_platform.machine())
    if identity["source"] != source_identity(repo) or identity["platform"] != platform.id:
        raise ValueError("candidate source or native platform differs from the gate")
    if expected_identity is not None and identity != expected_identity:
        raise ValueError("candidate source/toolchain/target/profile tuple differs from the build")
    rust, bun = pinned_toolchains(repo)
    if not identity["toolchains"]["rust"].startswith(f"rustc {rust} ") or identity["toolchains"]["bun"].split("+", 1)[0] != bun:
        raise ValueError("candidate toolchain differs from the source pins")
    target = identity["target"]
    compiler_hosts = [line.removeprefix("host: ") for line in identity["toolchains"]["rust"].splitlines()
                      if line.startswith("host: ")]
    native_targets = ({f"{platform.rust_arch}-apple-darwin"} if platform.system == "Darwin"
                      else {f"{platform.rust_arch}-unknown-linux-gnu", f"{platform.rust_arch}-unknown-linux-musl"})
    if compiler_hosts != [target] or target not in native_targets:
        raise ValueError("candidate compiler host or target differs from the native platform")
    if identity["profile"]["name"] != "release" or identity["profile"]["debug"] != 0:
        raise ValueError("candidate does not use the native release profile")
    if identity["profile"]["opt_level"] != ("3" if platform.system == "Darwin" else "s"):
        raise ValueError("candidate optimization differs from the native release profile")
    artifact_bundle.verify(root, identity["source"]["commit"], platform.id)
    release_root = contract.archive_root(identity["version"], platform.id)
    expected_paths = {member.id: f"{release_root}/{member.path}" for member in platform.archive_members}
    expected_paths["archive"] = f"{release_root}.tar.gz"
    if set(receipt["components"]) != set(expected_paths):
        raise ValueError("candidate component set differs from the release contract")
    for name, relative in expected_paths.items():
        component = receipt["components"][name]
        if component != {"path": relative, "bytes": (root / relative).stat().st_size,
                         "sha256": hash_file(root / relative)}:
            raise ValueError(f"candidate component differs: {name}")
    validate_build(contract, platform.id, *(root / expected_paths[name] for name in
                   ("engine", "wasm_host", "js_host", "opentui_native")))
    archive = root / expected_paths["archive"]
    verify_archive(contract, archive, identity["version"], platform.id)
    with tarfile.open(archive, "r:gz") as bundle:
        for name, relative in expected_paths.items():
            if name == "archive":
                continue
            stream = bundle.extractfile(relative)
            if stream is None:
                raise ValueError(f"candidate archive is missing component: {name}")
            with stream:
                digest = hashlib.sha256()
                while block := stream.read(1024 * 1024):
                    digest.update(block)
            if digest.hexdigest() != receipt["components"][name]["sha256"]:
                raise ValueError(f"candidate archive differs from staged component: {name}")
    return receipt


def component_path(root: Path, repo: Path, name: str) -> Path:
    receipt = verify(root, repo)
    if name not in receipt["components"]:
        raise ValueError(f"unknown candidate component: {name}")
    return root.absolute() / receipt["components"][name]["path"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("verify", "prepare", "path"))
    parser.add_argument("candidate", type=Path)
    parser.add_argument("component", nargs="?")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    receipt = verify(args.candidate, args.repo)
    if args.command == "path":
        if args.component not in receipt["components"]:
            parser.error("path requires a known candidate component")
        print(args.candidate.absolute() / receipt["components"][args.component]["path"])
    elif args.command == "prepare":
        # Artifact transport does not retain executable modes. Restore only the
        # verified release contract members, after checking all bytes.
        contract = load_contract(args.repo / "contracts/release-contract.json")
        platform = contract.platform(receipt["identity"]["platform"])
        for member in platform.archive_members:
            (args.candidate / receipt["components"][member.id]["path"]).chmod(member.mode)
        print(args.candidate.absolute())
    else:
        print(json.dumps({"identity_sha256": receipt["identity_sha256"],
                          "source": receipt["identity"]["source"],
                          "platform": receipt["identity"]["platform"],
                          "components": receipt["components"]}, sort_keys=True))


if __name__ == "__main__":
    main()
