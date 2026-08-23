#!/usr/bin/env python3
"""Parse, project, and verify Rottweiler's release product contract."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys
import tarfile
import tempfile
from typing import NoReturn


REPO = Path(__file__).resolve().parents[1]
DEFAULT_CONTRACT_PATH = REPO / "contracts" / "release-contract.json"
DEFAULT_RUST_OUTPUT = REPO / "crates" / "rw-types" / "src" / "generated" / "release_contract.rs"
DEFAULT_TYPESCRIPT_OUTPUT = REPO / "packages" / "tui" / "generated" / "release-contract.ts"
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?")
PLATFORM_PATTERN = re.compile(r"[a-z0-9]+-[a-z0-9_]+")
SAFE_PATH_PATTERN = re.compile(r"[A-Za-z0-9_.{}/-]+")
SAFE_LIBRARY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+")
EXPECTED_MEMBER_IDS = {
    "installer",
    "engine",
    "tui",
    "wasm_host",
    "plugin_host",
    "opentui_native",
}


@dataclass(frozen=True)
class ArchiveMember:
    id: str
    path: str
    mode: int
    max_bytes: int


@dataclass(frozen=True)
class ProductBudgets:
    engine_less_than_bytes: int
    wasm_host_less_than_bytes: int
    plugin_host_less_than_bytes: int
    tui_bundle_less_than_bytes: int


@dataclass(frozen=True)
class DistributionMetadata:
    operating_system: str
    label: str
    homebrew_condition: str


@dataclass(frozen=True)
class PlatformContract:
    id: str
    system: str
    machine: str
    rust_os: str
    rust_arch: str
    native_library: str
    product_budgets: ProductBudgets
    distribution: DistributionMetadata
    archive_members: tuple[ArchiveMember, ...]

    @property
    def uname_key(self) -> str:
        return f"{self.system}-{self.machine}"


@dataclass(frozen=True)
class ReleaseContract:
    schema_version: int
    root_format: str
    expanded_max_bytes: int
    platforms: tuple[PlatformContract, ...]

    def platform(self, platform_id: str) -> PlatformContract:
        for platform in self.platforms:
            if platform.id == platform_id:
                return platform
        raise ValueError(f"unsupported release platform: {platform_id}")

    def resolve_platform(self, system: str, machine: str) -> PlatformContract:
        for platform in self.platforms:
            if (platform.system, platform.machine) == (system, machine):
                return platform
        raise ValueError(f"unsupported release platform: {system}-{machine}")

    def archive_root(self, version: str, platform_id: str) -> str:
        _validate_version(version)
        platform = self.platform(platform_id)
        return self.root_format.format(version=version, platform=platform.id)


def _fail(path: str, message: str) -> NoReturn:
    raise ValueError(f"{path}: {message}")


def _object(value: object, path: str) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        _fail(path, "must be an object")
    return value


def _list(value: object, path: str) -> list[object]:
    if not isinstance(value, list):
        _fail(path, "must be an array")
    return value


def _string(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        _fail(path, "must be a non-empty string")
    return value


def _positive_integer(value: object, path: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        _fail(path, "must be a positive integer")
    return value


def _expect_keys(document: dict[str, object], path: str, expected: set[str]) -> None:
    unknown = sorted(set(document) - expected)
    missing = sorted(expected - set(document))
    if unknown:
        _fail(path, f"unknown field: {unknown[0]}")
    if missing:
        _fail(path, f"missing field: {missing[0]}")


def _validate_version(version: str) -> None:
    if VERSION_PATTERN.fullmatch(version) is None:
        raise ValueError("version must be a canonical semantic version without a leading v")


def _parse_member(value: object, index: int) -> ArchiveMember:
    path = f"archive.members[{index}]"
    document = _object(value, path)
    _expect_keys(document, path, {"id", "path", "mode", "max_bytes"})
    member_id = _string(document["id"], f"{path}.id")
    member_path = _string(document["path"], f"{path}.path")
    if SAFE_PATH_PATTERN.fullmatch(member_path) is None:
        _fail(f"{path}.path", "contains an unsafe character")
    pure_path = PurePosixPath(member_path)
    if pure_path.is_absolute() or ".." in pure_path.parts or "." in pure_path.parts:
        _fail(f"{path}.path", "must be a normalized relative path")
    if member_path.count("{native_library}") > 1 or "{" in member_path.replace(
        "{native_library}", ""
    ):
        _fail(f"{path}.path", "contains an unsupported template marker")
    mode_text = _string(document["mode"], f"{path}.mode")
    if mode_text not in {"0644", "0755"}:
        _fail(f"{path}.mode", "must be 0644 or 0755")
    return ArchiveMember(
        id=member_id,
        path=member_path,
        mode=int(mode_text, 8),
        max_bytes=_positive_integer(document["max_bytes"], f"{path}.max_bytes"),
    )


def _parse_budgets(value: object, path: str) -> ProductBudgets:
    document = _object(value, path)
    fields = {
        "engine_less_than_bytes",
        "wasm_host_less_than_bytes",
        "plugin_host_less_than_bytes",
        "tui_bundle_less_than_bytes",
    }
    _expect_keys(document, path, fields)
    return ProductBudgets(
        engine_less_than_bytes=_positive_integer(
            document["engine_less_than_bytes"], f"{path}.engine_less_than_bytes"
        ),
        wasm_host_less_than_bytes=_positive_integer(
            document["wasm_host_less_than_bytes"], f"{path}.wasm_host_less_than_bytes"
        ),
        plugin_host_less_than_bytes=_positive_integer(
            document["plugin_host_less_than_bytes"], f"{path}.plugin_host_less_than_bytes"
        ),
        tui_bundle_less_than_bytes=_positive_integer(
            document["tui_bundle_less_than_bytes"], f"{path}.tui_bundle_less_than_bytes"
        ),
    )


def _parse_distribution(value: object, path: str) -> DistributionMetadata:
    document = _object(value, path)
    _expect_keys(document, path, {"operating_system", "label", "homebrew_condition"})
    operating_system = _string(document["operating_system"], f"{path}.operating_system")
    if operating_system not in {"linux", "macos"}:
        _fail(f"{path}.operating_system", "must be linux or macos")
    return DistributionMetadata(
        operating_system=operating_system,
        label=_string(document["label"], f"{path}.label"),
        homebrew_condition=_string(
            document["homebrew_condition"], f"{path}.homebrew_condition"
        ),
    )


def load_contract(path: Path = DEFAULT_CONTRACT_PATH) -> ReleaseContract:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"could not read release contract {path}: {error}") from error
    document = _object(raw, "contract")
    _expect_keys(document, "contract", {"schema_version", "archive", "platforms"})
    if isinstance(document["schema_version"], bool) or document["schema_version"] != 1:
        _fail("schema_version", "must be 1")

    archive = _object(document["archive"], "archive")
    _expect_keys(archive, "archive", {"root_format", "expanded_max_bytes", "members"})
    root_format = _string(archive["root_format"], "archive.root_format")
    if root_format != "rottweiler-{version}-{platform}":
        _fail("archive.root_format", "must be rottweiler-{version}-{platform}")
    expanded_max_bytes = _positive_integer(
        archive["expanded_max_bytes"], "archive.expanded_max_bytes"
    )
    members = tuple(
        _parse_member(value, index)
        for index, value in enumerate(_list(archive["members"], "archive.members"))
    )
    member_ids = [member.id for member in members]
    if set(member_ids) != EXPECTED_MEMBER_IDS or len(member_ids) != len(EXPECTED_MEMBER_IDS):
        _fail("archive.members", "must define each required member id exactly once")
    template_paths = [member.path for member in members]
    if len(template_paths) != len(set(template_paths)):
        _fail("archive.members", "contains a duplicate member path")
    member_by_id = {member.id: member for member in members}
    if "{native_library}" not in member_by_id["opentui_native"].path:
        _fail("archive.members", "opentui_native path must contain {native_library}")
    if any(
        "{native_library}" in member.path
        for member in members
        if member.id != "opentui_native"
    ):
        _fail("archive.members", "only opentui_native may use {native_library}")

    platforms: list[PlatformContract] = []
    for index, value in enumerate(_list(document["platforms"], "platforms")):
        platform_path = f"platforms[{index}]"
        platform_document = _object(value, platform_path)
        _expect_keys(
            platform_document,
            platform_path,
            {
                "id",
                "system",
                "machine",
                "rust_os",
                "rust_arch",
                "native_library",
                "product_budgets",
                "distribution",
            },
        )
        platform_id = _string(platform_document["id"], f"{platform_path}.id")
        if PLATFORM_PATTERN.fullmatch(platform_id) is None:
            _fail(f"{platform_path}.id", "is not a canonical platform id")
        native_library = _string(
            platform_document["native_library"], f"{platform_path}.native_library"
        )
        if SAFE_LIBRARY_PATTERN.fullmatch(native_library) is None:
            _fail(f"{platform_path}.native_library", "is not a safe file name")
        resolved_members = tuple(
            ArchiveMember(
                id=member.id,
                path=member.path.replace("{native_library}", native_library),
                mode=member.mode,
                max_bytes=member.max_bytes,
            )
            for member in members
        )
        if len({member.path for member in resolved_members}) != len(resolved_members):
            _fail(platform_path, "resolves to duplicate archive member paths")
        platforms.append(
            PlatformContract(
                id=platform_id,
                system=_string(platform_document["system"], f"{platform_path}.system"),
                machine=_string(platform_document["machine"], f"{platform_path}.machine"),
                rust_os=_string(platform_document["rust_os"], f"{platform_path}.rust_os"),
                rust_arch=_string(platform_document["rust_arch"], f"{platform_path}.rust_arch"),
                native_library=native_library,
                product_budgets=_parse_budgets(
                    platform_document["product_budgets"], f"{platform_path}.product_budgets"
                ),
                distribution=_parse_distribution(
                    platform_document["distribution"], f"{platform_path}.distribution"
                ),
                archive_members=resolved_members,
            )
        )
    if not platforms:
        _fail("platforms", "must not be empty")
    identifiers = [platform.id for platform in platforms]
    if len(identifiers) != len(set(identifiers)):
        _fail("platforms", "duplicate platform id")
    probes = [(platform.system, platform.machine) for platform in platforms]
    if len(probes) != len(set(probes)):
        _fail("platforms", "duplicate uname probe")
    rust_targets = [(platform.rust_os, platform.rust_arch) for platform in platforms]
    if len(rust_targets) != len(set(rust_targets)):
        _fail("platforms", "duplicate Rust target")
    extraction_max_bytes = sum(member.max_bytes for member in members)
    if expanded_max_bytes > extraction_max_bytes:
        _fail(
            "archive.expanded_max_bytes",
            "must not exceed the sum of member extraction limits",
        )
    for platform in platforms:
        budgets = platform.product_budgets
        if budgets.engine_less_than_bytes > member_by_id["engine"].max_bytes:
            _fail(
                f"platforms.{platform.id}.product_budgets.engine_less_than_bytes",
                "engine product budget exceeds its member extraction limit",
            )
        if budgets.wasm_host_less_than_bytes > member_by_id["wasm_host"].max_bytes:
            _fail(
                f"platforms.{platform.id}.product_budgets.wasm_host_less_than_bytes",
                "WASM host product budget exceeds its member extraction limit",
            )
        if budgets.plugin_host_less_than_bytes > member_by_id["plugin_host"].max_bytes:
            _fail(
                f"platforms.{platform.id}.product_budgets.plugin_host_less_than_bytes",
                "plugin host product budget exceeds its member extraction limit",
            )
        tui_extraction_bytes = (
            member_by_id["tui"].max_bytes + member_by_id["opentui_native"].max_bytes
        )
        if budgets.tui_bundle_less_than_bytes > tui_extraction_bytes:
            _fail(
                f"platforms.{platform.id}.product_budgets.tui_bundle_less_than_bytes",
                "TUI bundle product budget exceeds its member extraction limits",
            )
        maximum_allowed_product_bytes = (
            member_by_id["installer"].max_bytes
            + budgets.engine_less_than_bytes
            + budgets.wasm_host_less_than_bytes
            + budgets.plugin_host_less_than_bytes
            + budgets.tui_bundle_less_than_bytes
        )
        if maximum_allowed_product_bytes > expanded_max_bytes:
            _fail(
                f"platforms.{platform.id}.product_budgets",
                "product budgets exceed expanded archive limit",
            )

    return ReleaseContract(
        schema_version=1,
        root_format=root_format,
        expanded_max_bytes=expanded_max_bytes,
        platforms=tuple(platforms),
    )


def _member_by_id(platform: PlatformContract, member_id: str) -> ArchiveMember:
    for member in platform.archive_members:
        if member.id == member_id:
            return member
    raise ValueError(f"release contract is missing archive member {member_id}")


def validate_build(
    contract: ReleaseContract,
    platform_id: str,
    engine: Path,
    wasm_host: Path,
    plugin_host: Path,
    tui: Path,
    opentui_native: Path,
) -> None:
    platform = contract.platform(platform_id)
    paths = {
        "engine": engine,
        "wasm_host": wasm_host,
        "plugin_host": plugin_host,
        "tui": tui,
        "opentui_native": opentui_native,
    }
    sizes: dict[str, int] = {}
    for member_id, path in paths.items():
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ValueError(f"release {member_id} is unavailable: {path}: {error}") from error
        if (
            path.is_symlink()
            or not path.is_file()
            or metadata.st_nlink != 1
            or metadata.st_size == 0
        ):
            raise ValueError(f"release {member_id} must be a single-link regular file: {path}")
        sizes[member_id] = metadata.st_size
        extraction_limit = _member_by_id(platform, member_id).max_bytes
        if metadata.st_size > extraction_limit:
            raise ValueError(
                f"release {member_id} is {metadata.st_size} bytes; extraction limit is "
                f"{extraction_limit}"
            )
    budgets = platform.product_budgets
    checks = (
        ("engine", sizes["engine"], budgets.engine_less_than_bytes),
        ("WASM helper", sizes["wasm_host"], budgets.wasm_host_less_than_bytes),
        ("TypeScript plugin host", sizes["plugin_host"], budgets.plugin_host_less_than_bytes),
        (
            "TUI bundle",
            sizes["tui"] + sizes["opentui_native"],
            budgets.tui_bundle_less_than_bytes,
        ),
    )
    for label, actual, exclusive_limit in checks:
        if actual >= exclusive_limit:
            raise ValueError(
                f"release {label} is {actual} bytes; product budget is <{exclusive_limit}"
            )


def _archive_directories(platform: PlatformContract) -> tuple[str, ...]:
    directories: set[str] = set()
    for member in platform.archive_members:
        parent = PurePosixPath(member.path).parent
        while str(parent) != ".":
            directories.add(str(parent))
            parent = parent.parent
    return tuple(sorted(directories, key=lambda item: (item.count("/"), item)))


def verify_archive(
    contract: ReleaseContract, archive: Path, version: str, platform_id: str
) -> None:
    platform = contract.platform(platform_id)
    release_root = contract.archive_root(version, platform_id)
    expected_name = f"{release_root}.tar.gz"
    if archive.name != expected_name:
        raise ValueError(f"release archive must be named {expected_name}: {archive}")
    expected: dict[str, tuple[str, int, int]] = {
        release_root: ("directory", 0o755, 0),
    }
    for directory in _archive_directories(platform):
        expected[f"{release_root}/{directory}"] = ("directory", 0o755, 0)
    for member in platform.archive_members:
        expected[f"{release_root}/{member.path}"] = ("file", member.mode, member.max_bytes)
    try:
        with tarfile.open(archive, "r:gz") as bundle:
            members = bundle.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise ValueError(f"could not read release archive {archive}: {error}") from error
    names = [member.name.rstrip("/") for member in members]
    if len(names) != len(set(names)) or set(names) != set(expected) or len(names) != len(expected):
        raise ValueError("release archive does not match the exact contract shape")
    expanded_bytes = 0
    for member, name in zip(members, names, strict=True):
        pure_path = PurePosixPath(member.name)
        if pure_path.is_absolute() or ".." in pure_path.parts or "." in pure_path.parts:
            raise ValueError(f"release archive contains an unsafe path: {member.name}")
        canonical_name = pure_path.as_posix()
        allowed_names = {canonical_name, f"{canonical_name}/"} if member.isdir() else {canonical_name}
        if member.name not in allowed_names:
            raise ValueError(f"release archive contains a non-canonical path: {member.name}")
        expected_kind, expected_mode, maximum_bytes = expected[name]
        actual_kind = "directory" if member.isdir() else "file" if member.isfile() else "other"
        if actual_kind != expected_kind:
            raise ValueError(f"release archive entry has the wrong type: {member.name}")
        if member.mode & 0o7777 != expected_mode:
            raise ValueError(f"release archive entry has the wrong mode: {member.name}")
        if member.size < 0 or member.size > maximum_bytes or (member.isfile() and member.size == 0):
            raise ValueError(f"release archive entry exceeds its size bound: {member.name}")
        expanded_bytes += member.size
    if expanded_bytes > contract.expanded_max_bytes:
        raise ValueError(
            f"release archive expands to {expanded_bytes} bytes; limit is "
            f"{contract.expanded_max_bytes}"
        )


def _rust_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=True)


def _rust_integer(value: int) -> str:
    return f"{value:_}"


def render_rust(contract: ReleaseContract) -> str:
    lines = [
        "// @generated by scripts/release_contract.py; do not edit.",
        "",
        "#[derive(Clone, Copy, Debug, Eq, PartialEq)]",
        "pub struct ReleaseArchiveMember {",
        "    pub id: &'static str,",
        "    pub path: &'static str,",
        "    pub mode: u32,",
        "    pub max_bytes: u64,",
        "}",
        "",
        "#[derive(Clone, Copy, Debug, Eq, PartialEq)]",
        "pub struct ReleaseProductBudgets {",
        "    pub engine_less_than_bytes: u64,",
        "    pub wasm_host_less_than_bytes: u64,",
        "    pub plugin_host_less_than_bytes: u64,",
        "    pub tui_bundle_less_than_bytes: u64,",
        "}",
        "",
        "#[derive(Clone, Copy, Debug, Eq, PartialEq)]",
        "pub struct ReleasePlatform {",
        "    pub id: &'static str,",
        "    pub system: &'static str,",
        "    pub machine: &'static str,",
        "    pub rust_os: &'static str,",
        "    pub rust_arch: &'static str,",
        "    pub native_library: &'static str,",
        "    pub product_budgets: ReleaseProductBudgets,",
        "    pub archive_members: &'static [ReleaseArchiveMember],",
        "}",
        "",
        "pub const EXPANDED_ARCHIVE_MAX_BYTES: u64 = "
        f"{_rust_integer(contract.expanded_max_bytes)};",
        "",
        "#[must_use]",
        "pub fn archive_root(version: &str, platform: &str) -> String {",
        f"    format!({_rust_string(contract.root_format)})",
        "}",
        "",
    ]
    constant_names: dict[str, str] = {}
    for platform in contract.platforms:
        constant_name = re.sub(r"[^A-Za-z0-9]", "_", platform.id).upper()
        constant_names[platform.id] = constant_name
        lines.append(f"const {constant_name}_ARCHIVE_MEMBERS: &[ReleaseArchiveMember] = &[")
        for member in platform.archive_members:
            lines.extend(
                [
                    "    ReleaseArchiveMember {",
                    f"        id: {_rust_string(member.id)},",
                    f"        path: {_rust_string(member.path)},",
                    f"        mode: 0o{member.mode:o},",
                    f"        max_bytes: {_rust_integer(member.max_bytes)},",
                    "    },",
                ]
            )
        lines.extend(["];", ""])
    lines.append("pub const RELEASE_PLATFORMS: &[ReleasePlatform] = &[")
    for platform in contract.platforms:
        budgets = platform.product_budgets
        constant_name = constant_names[platform.id]
        lines.extend(
            [
                "    ReleasePlatform {",
                f"        id: {_rust_string(platform.id)},",
                f"        system: {_rust_string(platform.system)},",
                f"        machine: {_rust_string(platform.machine)},",
                f"        rust_os: {_rust_string(platform.rust_os)},",
                f"        rust_arch: {_rust_string(platform.rust_arch)},",
                f"        native_library: {_rust_string(platform.native_library)},",
                "        product_budgets: ReleaseProductBudgets {",
                "            engine_less_than_bytes: "
                f"{_rust_integer(budgets.engine_less_than_bytes)},",
                "            wasm_host_less_than_bytes: "
                f"{_rust_integer(budgets.wasm_host_less_than_bytes)},",
                "            plugin_host_less_than_bytes: "
                f"{_rust_integer(budgets.plugin_host_less_than_bytes)},",
                "            tui_bundle_less_than_bytes: "
                f"{_rust_integer(budgets.tui_bundle_less_than_bytes)},",
                "        },",
                f"        archive_members: {constant_name}_ARCHIVE_MEMBERS,",
                "    },",
            ]
        )
    lines.extend(
        [
            "];",
            "",
            "#[must_use]",
            "pub fn platform_by_id(id: &str) -> Option<&'static ReleasePlatform> {",
            "    RELEASE_PLATFORMS.iter().find(|platform| platform.id == id)",
            "}",
            "",
            "#[must_use]",
            "pub fn platform_for_rust_target(os: &str, arch: &str) "
            "-> Option<&'static ReleasePlatform> {",
            "    RELEASE_PLATFORMS",
            "        .iter()",
            "        .find(|platform| platform.rust_os == os && platform.rust_arch == arch)",
            "}",
            "",
        ]
    )
    return "\n".join(lines)


def render_typescript(contract: ReleaseContract) -> str:
    node_platforms = {"macos": "darwin", "linux": "linux"}
    node_arches = {"aarch64": "arm64", "x86_64": "x64"}
    lines = [
        "// @generated TypeScript projection by scripts/release_contract.py; do not edit.",
        "",
        "export interface ReleaseProductBudgets {",
        "  readonly engineLessThanBytes: number",
        "  readonly wasmHostLessThanBytes: number",
        "  readonly pluginHostLessThanBytes: number",
        "  readonly tuiBundleLessThanBytes: number",
        "}",
        "",
        "export interface ReleasePlatform {",
        "  readonly id: string",
        "  readonly nodePlatform: string",
        "  readonly nodeArch: string",
        "  readonly nativeLibrary: string",
        "  readonly productBudgets: ReleaseProductBudgets",
        "}",
        "",
        "export const RELEASE_PLATFORMS = [",
    ]
    for platform in contract.platforms:
        try:
            node_platform = node_platforms[platform.rust_os]
            node_arch = node_arches[platform.rust_arch]
        except KeyError as error:
            raise ValueError(
                f"platform {platform.id} has no Node target projection"
            ) from error
        budgets = platform.product_budgets
        lines.extend(
            [
                "  {",
                f"    id: {_rust_string(platform.id)},",
                f"    nodePlatform: {_rust_string(node_platform)},",
                f"    nodeArch: {_rust_string(node_arch)},",
                f"    nativeLibrary: {_rust_string(platform.native_library)},",
                "    productBudgets: {",
                f"      engineLessThanBytes: {budgets.engine_less_than_bytes},",
                f"      wasmHostLessThanBytes: {budgets.wasm_host_less_than_bytes},",
                f"      pluginHostLessThanBytes: {budgets.plugin_host_less_than_bytes},",
                f"      tuiBundleLessThanBytes: {budgets.tui_bundle_less_than_bytes},",
                "    },",
                "  },",
            ]
        )
    lines.extend(
        [
            "] as const satisfies readonly ReleasePlatform[]",
            "",
            "export function releasePlatformForNodeTarget(",
            "  nodePlatform: string,",
            "  nodeArch: string,",
            "): ReleasePlatform | undefined {",
            "  return RELEASE_PLATFORMS.find(",
            "    (platform) =>",
            "      platform.nodePlatform === nodePlatform && platform.nodeArch === nodeArch,",
            "  )",
            "}",
            "",
        ]
    )
    return "\n".join(lines)


def _write_atomic(path: Path, content: str, mode: int = 0o644) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(mode)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _shell_words(values: list[str]) -> str:
    for value in values:
        if re.fullmatch(r"[A-Za-z0-9_./-]+", value) is None:
            raise ValueError(f"release contract path cannot be rendered safely in shell: {value}")
    return " ".join(values)


def render_installer(
    contract: ReleaseContract,
    template_path: Path,
    version: str,
    platform_id: str,
) -> str:
    platform = contract.platform(platform_id)
    _validate_version(version)
    members = list(platform.archive_members)
    archive_files = [member.path for member in members]
    archive_directories = list(_archive_directories(platform))
    executable_files = [member.path for member in members if member.mode & 0o111]
    readonly_files = [member.path for member in members if not member.mode & 0o111]
    engine_path = _member_by_id(platform, "engine").path

    def sync_arguments(prefix: str) -> str:
        paths = [f'    "{prefix}/{member.path}" \\' for member in members]
        paths.extend(f'    "{prefix}/{directory}" \\' for directory in archive_directories)
        return "\n".join(paths)

    replacements = {
        "@ROTTWEILER_VERSION@": version,
        "@ROTTWEILER_PLATFORM@": platform.id,
        "@ROTTWEILER_RELEASE_ROOT@": contract.archive_root(version, platform.id),
        "@ROTTWEILER_ARCHIVE_FILES@": _shell_words(archive_files),
        "@ROTTWEILER_ARCHIVE_DIRECTORIES@": _shell_words(archive_directories),
        "@ROTTWEILER_EXECUTABLE_FILES@": _shell_words(executable_files),
        "@ROTTWEILER_READONLY_FILES@": _shell_words(readonly_files),
        "@ROTTWEILER_ARCHIVE_ENTRY_COUNT@": str(len(archive_files) + len(archive_directories)),
        "@ROTTWEILER_ENGINE_PATH@": engine_path,
        "@ROTTWEILER_STAGING_SYNC_ARGUMENTS@": sync_arguments("$staging"),
        "@ROTTWEILER_VERSION_SYNC_ARGUMENTS@": sync_arguments("$version_dir"),
    }
    try:
        rendered = template_path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"could not read installer template {template_path}: {error}") from error
    for marker, value in replacements.items():
        if rendered.count(marker) != 1:
            raise ValueError(f"installer template marker must occur exactly once: {marker}")
        rendered = rendered.replace(marker, value)
    unresolved = re.search(r"@[A-Z][A-Z_]+@", rendered)
    if unresolved is not None:
        raise ValueError(f"installer template contains unresolved marker: {unresolved.group(0)}")
    return rendered


def stage_release(
    contract: ReleaseContract,
    output: Path,
    template_path: Path,
    version: str,
    platform_id: str,
    engine: Path,
    wasm_host: Path,
    plugin_host: Path,
    tui: Path,
    opentui_native: Path,
) -> None:
    platform = contract.platform(platform_id)
    expected_root = contract.archive_root(version, platform_id)
    if output.name != expected_root:
        raise ValueError(f"release stage must be named {expected_root}: {output}")
    if output.exists() or output.is_symlink():
        raise ValueError(f"release stage already exists: {output}")
    validate_build(contract, platform_id, engine, wasm_host, plugin_host, tui, opentui_native)
    sources = {
        "engine": engine,
        "wasm_host": wasm_host,
        "plugin_host": plugin_host,
        "tui": tui,
        "opentui_native": opentui_native,
    }
    output.mkdir(parents=True, mode=0o755)
    for directory in _archive_directories(platform):
        destination = output / directory
        destination.mkdir(mode=0o755)
        destination.chmod(0o755)
    for member in platform.archive_members:
        destination = output / member.path
        if member.id == "installer":
            _write_atomic(
                destination,
                render_installer(contract, template_path, version, platform_id),
                member.mode,
            )
        else:
            source = sources[member.id]
            shutil.copyfile(source, destination)
        destination.chmod(member.mode)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--contract", type=Path, default=DEFAULT_CONTRACT_PATH)
    subparsers = parser.add_subparsers(dest="command", required=True)

    resolve = subparsers.add_parser("resolve-platform")
    resolve.add_argument("--system", required=True)
    resolve.add_argument("--machine", required=True)

    platform_field = subparsers.add_parser("platform-field")
    platform_field.add_argument("--platform", required=True)
    platform_field.add_argument(
        "--field", required=True, choices=("native-library", "uname-key")
    )

    archive_root = subparsers.add_parser("archive-root")
    archive_root.add_argument("--version", required=True)
    archive_root.add_argument("--platform", required=True)

    member_path = subparsers.add_parser("member-path")
    member_path.add_argument("--platform", required=True)
    member_path.add_argument("--member", required=True, choices=tuple(sorted(EXPECTED_MEMBER_IDS)))

    validate = subparsers.add_parser("validate-build")
    validate.add_argument("--platform", required=True)
    validate.add_argument("--engine", required=True, type=Path)
    validate.add_argument("--wasm-host", required=True, type=Path)
    validate.add_argument("--plugin-host", required=True, type=Path)
    validate.add_argument("--tui", required=True, type=Path)
    validate.add_argument("--opentui-native", required=True, type=Path)

    installer = subparsers.add_parser("render-installer")
    installer.add_argument("--template", required=True, type=Path)
    installer.add_argument("--version", required=True)
    installer.add_argument("--platform", required=True)
    installer.add_argument("--output", required=True, type=Path)

    stage = subparsers.add_parser("stage-release")
    stage.add_argument("--output", required=True, type=Path)
    stage.add_argument("--template", required=True, type=Path)
    stage.add_argument("--version", required=True)
    stage.add_argument("--platform", required=True)
    stage.add_argument("--engine", required=True, type=Path)
    stage.add_argument("--wasm-host", required=True, type=Path)
    stage.add_argument("--plugin-host", required=True, type=Path)
    stage.add_argument("--tui", required=True, type=Path)
    stage.add_argument("--opentui-native", required=True, type=Path)

    verify = subparsers.add_parser("verify-archive")
    verify.add_argument("--archive", required=True, type=Path)
    verify.add_argument("--version", required=True)
    verify.add_argument("--platform", required=True)

    generate = subparsers.add_parser("generate-rust")
    generate.add_argument("--output", type=Path, default=DEFAULT_RUST_OUTPUT)
    generate.add_argument("--check", action="store_true")
    generate_typescript = subparsers.add_parser("generate-typescript")
    generate_typescript.add_argument("--output", type=Path, default=DEFAULT_TYPESCRIPT_OUTPUT)
    generate_typescript.add_argument("--check", action="store_true")
    return parser.parse_args()


def run(args: argparse.Namespace) -> None:
    contract = load_contract(args.contract)
    if args.command == "resolve-platform":
        print(contract.resolve_platform(args.system, args.machine).id)
    elif args.command == "platform-field":
        platform = contract.platform(args.platform)
        print(platform.native_library if args.field == "native-library" else platform.uname_key)
    elif args.command == "archive-root":
        print(contract.archive_root(args.version, args.platform))
    elif args.command == "member-path":
        print(_member_by_id(contract.platform(args.platform), args.member).path)
    elif args.command == "validate-build":
        validate_build(
            contract,
            args.platform,
            args.engine,
            args.wasm_host,
            args.plugin_host,
            args.tui,
            args.opentui_native,
        )
    elif args.command == "render-installer":
        _write_atomic(
            args.output,
            render_installer(contract, args.template, args.version, args.platform),
        )
        args.output.chmod(0o755)
    elif args.command == "stage-release":
        stage_release(
            contract,
            args.output,
            args.template,
            args.version,
            args.platform,
            args.engine,
            args.wasm_host,
            args.plugin_host,
            args.tui,
            args.opentui_native,
        )
    elif args.command == "verify-archive":
        verify_archive(contract, args.archive, args.version, args.platform)
    elif args.command == "generate-rust":
        rendered = render_rust(contract)
        if args.check:
            try:
                current = args.output.read_text(encoding="utf-8")
            except OSError as error:
                raise ValueError(f"generated Rust projection is missing: {args.output}: {error}") from error
            if current != rendered:
                raise ValueError(
                    "generated Rust projection is stale; run "
                    "python3 scripts/release_contract.py generate-rust"
                )
        else:
            _write_atomic(args.output, rendered)
    elif args.command == "generate-typescript":
        rendered = render_typescript(contract)
        if args.check:
            try:
                current = args.output.read_text(encoding="utf-8")
            except OSError as error:
                raise ValueError(
                    f"generated TypeScript projection is missing: {args.output}: {error}"
                ) from error
            if current != rendered:
                raise ValueError(
                    "generated TypeScript projection is stale; run "
                    "python3 scripts/release_contract.py generate-typescript"
                )
        else:
            _write_atomic(args.output, rendered)
    else:
        raise AssertionError(f"unhandled command: {args.command}")


def main() -> int:
    args = parse_args()
    try:
        run(args)
    except ValueError as error:
        print(f"release contract: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
