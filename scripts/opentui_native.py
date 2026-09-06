"""Pinned renderer source, allocator ownership and native artifact provenance."""
from __future__ import annotations

import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

from release_contract import load_contract

CONTRACT = "contracts/opentui-native.json"
PROBE = "packages/tui/scripts/native-lifetime-probe.ts"
RECEIPT = "native-build.json"


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def contract(repo: Path) -> dict:
    data = json.loads((repo / CONTRACT).read_text())
    if set(data) != {"schema_version", "package_version", "source", "zig", "patches", "flags"} or data["schema_version"] != 1:
        raise ValueError("invalid OpenTUI native contract")
    packages = [json.loads((repo / f"packages/{name}/package.json").read_text()) for name in ("tui",)]
    if any(p["dependencies"].get("@opentui/core") != data["package_version"] for p in packages):
        raise ValueError("OpenTUI native source version differs from JavaScript dependency")
    release = load_contract(repo / "contracts/release-contract.json")
    if set(data["zig"]["artifacts"]) != {p.id for p in release.platforms}:
        raise ValueError("Zig resources must cover exactly the release platforms")
    source = data["source"]
    if re.fullmatch(r"[a-f0-9]{40}", source["commit"]) is None or source["url"] != f"https://codeload.github.com/anomalyco/opentui/tar.gz/{source['commit']}":
        raise ValueError("OpenTUI source must be an exact upstream commit")
    for artifact in [source, *data["zig"]["artifacts"].values()]:
        if re.fullmatch(r"[a-f0-9]{64}", artifact["sha256"]) is None or not 0 < artifact["bytes"] <= 128 * 1024 * 1024:
            raise ValueError("native source/toolchain requires bounded hashed archive")
    for artifact in data["zig"]["artifacts"].values():
        if not artifact["url"].startswith(f"https://ziglang.org/download/{data['zig']['version']}/zig-"):
            raise ValueError("Zig must use the pinned official release")
    return data


def sdk_identity(platform_id: str) -> dict | None:
    if not platform_id.startswith("darwin-"):
        return None
    if platform.system() != "Darwin":
        raise ValueError("macOS native renderer requires a native macOS SDK")
    path = Path(subprocess.check_output(["xcrun", "--sdk", "macosx", "--show-sdk-path"], text=True).strip()).resolve()
    return {"path": str(path), "settings_sha256": digest(path / "SDKSettings.json")}


def configuration(repo: Path, platform_id: str) -> dict:
    data = contract(repo)
    host = load_contract(repo / "contracts/release-contract.json").platform(platform_id)
    notice = next(member for member in host.archive_members if member.id == "opentui_licenses")
    return {"builder_sha256": digest(Path(__file__)), "probe_sha256": digest(repo / PROBE), "bun": (repo / ".bun-version").read_text().strip(), "library": host.native_library, "licenses": Path(notice.path).name, "source": data["source"], "package_version": data["package_version"],
            "zig": {"version": data["zig"]["version"], "artifact": data["zig"]["artifacts"][platform_id]},
            "patches": {name: digest(repo / name) for name in data["patches"]},
            "flags": data["flags"], "platform": platform_id}


def identity(repo: Path, platform_id: str) -> dict:
    return {**configuration(repo, platform_id), "sdk": sdk_identity(platform_id)}


def validate_identity(repo: Path, platform_id: str, observed: object) -> None:
    if not isinstance(observed, dict) or "sdk" not in observed:
        raise ValueError("native build identity requires captured SDK provenance")
    source_fields = {key: value for key, value in observed.items() if key != "sdk"}
    if source_fields != configuration(repo, platform_id):
        raise ValueError("candidate native renderer differs from source/toolchain/allocator contract")
    sdk = observed["sdk"]
    if platform_id.startswith("darwin-"):
        if not isinstance(sdk, dict) or set(sdk) != {"path", "settings_sha256"} or not Path(sdk["path"]).is_absolute() or re.fullmatch(r"[a-f0-9]{64}", sdk["settings_sha256"]) is None:
            raise ValueError("macOS native build requires exact SDK provenance")
    elif sdk is not None:
        raise ValueError("Linux native build does not consume a macOS SDK")


def download(artifact: dict, directory: Path, supplied: Path | None = None) -> Path:
    path = directory / artifact["sha256"]
    if supplied is not None:
        if supplied.stat().st_size != artifact["bytes"] or digest(supplied) != artifact["sha256"]:
            raise ValueError("supplied native toolchain archive does not match source identity")
        return supplied
    if path.exists():
        if path.stat().st_size != artifact["bytes"] or digest(path) != artifact["sha256"]:
            raise ValueError("cached native archive is corrupt")
        return path
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=directory, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            with urllib.request.urlopen(artifact["url"], timeout=60) as response:
                remaining = artifact["bytes"]
                while block := response.read(min(1024 * 1024, remaining + 1)):
                    remaining -= len(block)
                    if remaining < 0:
                        raise ValueError("native archive exceeds its exact byte contract")
                    stream.write(block)
            stream.flush()
            if remaining != 0 or digest(temporary) != artifact["sha256"]:
                raise ValueError("native archive identity mismatch")
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)
    return path


def extract(archive: Path, directory: Path) -> Path:
    directory.mkdir()
    with tarfile.open(archive) as source:
        members = source.getmembers()
        if len(members) > 100_000 or sum(member.size for member in members) > 768 * 1024 * 1024:
            raise ValueError("native archive expanded content exceeds its extraction allowance")
        source.extractall(directory, members=members, filter="data")
    entries = list(directory.iterdir())
    if len(entries) != 1 or not entries[0].is_dir():
        raise ValueError("native source/toolchain must have one archive root")
    return entries[0]


def verify(directory: Path, expected: dict) -> Path:
    receipt_path = directory / RECEIPT
    if receipt_path.is_symlink() or receipt_path.stat().st_size > 128 * 1024:
        raise ValueError("native renderer receipt must be bounded regular data")
    receipt = json.loads(receipt_path.read_text())
    if set(receipt) != {"identity", "library", "sha256", "licenses", "probe_sha256"}:
        raise ValueError("invalid native renderer receipt")
    name = expected["library"]
    if receipt["library"] != name:
        raise ValueError("native renderer receipt has an unexpected library")
    library = directory / name
    if receipt["identity"] != expected or library.is_symlink() or digest(library) != receipt["sha256"]:
        raise ValueError("native renderer build receipt does not match source or artifact")
    if expected["licenses"] not in receipt["licenses"]:
        raise ValueError("native renderer receipt omits required license notice")
    if digest(directory / "lifetime-probe.json") != receipt["probe_sha256"]:
        raise ValueError("native renderer lifetime proof identity mismatch")
    for name, checksum in receipt["licenses"].items():
        if Path(name).is_absolute() or ".." in Path(name).parts or (directory / name).is_symlink():
            raise ValueError("native renderer license path escapes its artifact")
        if digest(directory / name) != checksum:
            raise ValueError("native renderer license inventory mismatch")
    return library


def cache_root(target: Path) -> Path:
    supplied = os.environ.get("ROTTWEILER_NATIVE_CACHE_DIR")
    root = target / "opentui-native" if supplied is None else Path(supplied)
    if not root.is_absolute():
        raise ValueError("native cache root must be an absolute directory")
    if root.is_symlink():
        raise ValueError("native cache root cannot be a symlink")
    root.mkdir(parents=True, exist_ok=True)
    try:
        with (root / "CACHEDIR.TAG").open("x") as tag:
            tag.write("Signature: 8a477f597d28d172789f06886806bc55\n# Rottweiler native renderer cache.\n")
    except FileExistsError:
        pass
    return root


def cached_library(destination: Path, expected: dict) -> Path | None:
    """Recover only an invalid builder-owned key while its build lock is held."""
    if not destination.exists() and not destination.is_symlink():
        return None
    try:
        if destination.is_symlink():
            raise ValueError("native cache entry cannot be a symlink")
        return verify(destination, expected)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Rebuilding invalid native renderer cache {destination}: {error}", file=sys.stderr)
        if destination.is_symlink() or not destination.is_dir():
            destination.unlink()
        else:
            shutil.rmtree(destination)
        return None


def build(repo: Path, target: Path) -> Path:
    release = load_contract(repo / "contracts/release-contract.json")
    host = release.resolve_platform(platform.system(), platform.machine())
    expected = identity(repo, host.id)
    if subprocess.check_output(["bun", "--version"], text=True).strip() != expected["bun"]:
        raise ValueError("native lifetime acceptance requires the source-pinned Bun version")
    key = hashlib.sha256(json.dumps(expected, sort_keys=True).encode()).hexdigest()
    root = cache_root(target)
    with (root / ".build.lock").open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        destination = root / key
        cached = cached_library(destination, expected)
        if cached is not None:
            return cached
        archives = root / "archives"
        source_archive = download(expected["source"], archives)
        supplied = os.environ.get("ROTTWEILER_ZIG_ARCHIVE")
        zig_archive = download(expected["zig"]["artifact"], archives, Path(supplied) if supplied else None)
        with tempfile.TemporaryDirectory(prefix=".build-", dir=root) as temporary:
            work = Path(temporary)
            source = extract(source_archive, work / "source")
            zig_root = extract(zig_archive, work / "zig")
            zig = zig_root / "zig"
            if subprocess.check_output([str(zig), "version"], text=True).strip() != expected["zig"]["version"]:
                raise ValueError("native compiler version mismatch")
            if (source / ".zig-version").read_text().strip() != expected["zig"]["version"]:
                raise ValueError("upstream requires a different Zig version")
            if json.loads((source / "packages/core/package.json").read_text())["version"] != expected["package_version"]:
                raise ValueError("upstream source package version mismatch")
            # Isolate patch paths from the containing Rottweiler checkout: git
            # apply in an unversioned target subdirectory can otherwise skip them.
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            for patch in expected["patches"]:
                subprocess.run(["git", "apply", "--check", str(repo / patch)], cwd=source, check=True)
                subprocess.run(["git", "apply", str(repo / patch)], cwd=source, check=True)
            native = source / "packages/native"
            subprocess.run(["sh", "scripts/prepare-zig-deps.sh"], cwd=native, check=True)
            environment = dict(os.environ, ZIG_GLOBAL_CACHE_DIR=str(root / "cache"))
            # All dependency sources are in the checksum-covered upstream archive.
            flags = [*expected["flags"]]
            if expected["sdk"] is not None:
                flags.append(f"-Dmacos-sdk={expected['sdk']['path']}")
            subprocess.run([str(zig), "build", "--system", str(native / "zig-deps"), *flags], cwd=native, env=environment, check=True)
            produced = native / "lib" / f"{host.rust_arch}-{'macos' if host.system == 'Darwin' else 'linux'}" / host.native_library
            result = work / "result"
            result.mkdir()
            shutil.copy2(produced, result / host.native_library)
            with (result / "lifetime-probe.json").open("w") as report:
                subprocess.run(["bun", str(repo / PROBE), str(result / host.native_library)],
                               cwd=repo / "packages/tui", stdout=report, check=True)

            licenses = {}
            for item in source.rglob("*"):
                if item.is_file() and (item.name.upper().startswith(("LICENSE", "COPYING", "NOTICE"))) and not item.is_relative_to(native / ".zig-cache"):
                    relative = Path("licenses") / item.relative_to(source)
                    output = result / relative
                    output.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(item, output)
                    licenses[str(relative)] = digest(output)
            license_member = next(member for member in host.archive_members if member.id == "opentui_licenses")
            license_name = Path(license_member.path).name
            notice = "OpenTUI native source and bundled dependency licenses\n"
            notice += f"Upstream commit: {expected['source']['commit']}\n"
            notice += "Allocator ownership changes: " + ", ".join(expected["patches"]) + "\n\n"
            for name in sorted(licenses):
                notice += f"===== {name} =====\n" + (result / name).read_text() + "\n"
            if len(notice.encode()) > license_member.max_bytes:
                raise ValueError("native license notice exceeds release member limit")
            (result / license_name).write_text(notice)
            licenses[license_name] = digest(result / license_name)
            (result / RECEIPT).write_text(json.dumps({"identity": expected, "library": host.native_library,
                "sha256": digest(result / host.native_library), "licenses": licenses, "probe_sha256": digest(result / "lifetime-probe.json")}, sort_keys=True, indent=2) + "\n")
            os.rename(result, destination)
        return verify(destination, expected)
