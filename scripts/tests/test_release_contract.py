from __future__ import annotations

import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
CONTRACT_PATH = REPO / "contracts" / "release-contract.json"
SCRIPT = REPO / "scripts" / "release_contract.py"
GENERATED_RUST = REPO / "crates" / "rw-types" / "src" / "generated" / "release_contract.rs"
GENERATED_TYPESCRIPT = REPO / "packages" / "tui" / "generated" / "release-contract.ts"
VERSION = "1.2.3"


def load_module():
    spec = importlib.util.spec_from_file_location("release_contract", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load release contract module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def make_archive(path: Path, contract, platform_id: str, *, extra: bool = False) -> None:
    platform_contract = contract.platform(platform_id)
    root = contract.archive_root(VERSION, platform_id)
    with tarfile.open(path, "w:gz") as archive:
        root_info = tarfile.TarInfo(root)
        root_info.type = tarfile.DIRTYPE
        root_info.mode = 0o755
        archive.addfile(root_info)
        bin_info = tarfile.TarInfo(f"{root}/bin")
        bin_info.type = tarfile.DIRTYPE
        bin_info.mode = 0o755
        archive.addfile(bin_info)
        for member in platform_contract.archive_members:
            content = b"#!/bin/sh\n" if member.mode == 0o755 else b"native\n"
            info = tarfile.TarInfo(f"{root}/{member.path}")
            info.mode = member.mode
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
        if extra:
            content = b"unexpected\n"
            info = tarfile.TarInfo(f"{root}/extra")
            info.mode = 0o644
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))


class ReleaseContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_contract_resolves_canonical_platforms_and_product_budgets(self) -> None:
        contract = self.module.load_contract(CONTRACT_PATH)
        self.assertEqual(contract.resolve_platform("Linux", "aarch64").id, "linux-aarch64")
        self.assertEqual(contract.resolve_platform("Darwin", "arm64").id, "darwin-arm64")
        self.assertEqual(
            contract.platform("darwin-arm64").product_budgets.engine_less_than_bytes,
            40_000_000,
        )
        self.assertEqual(
            contract.platform("linux-x86_64").product_budgets.engine_less_than_bytes,
            28_000_000,
        )
        self.assertEqual(contract.platform("linux-aarch64").native_library, "libopentui.so")

    def test_contract_rejects_unknown_fields_and_duplicate_platform_ids(self) -> None:
        document = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "contract.json"
            unknown = copy.deepcopy(document)
            unknown["unexpected"] = True
            path.write_text(json.dumps(unknown), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "unknown field"):
                self.module.load_contract(path)

            duplicate = copy.deepcopy(document)
            duplicate["platforms"].append(copy.deepcopy(duplicate["platforms"][0]))
            path.write_text(json.dumps(duplicate), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate platform id"):
                self.module.load_contract(path)

    def test_contract_rejects_incoherent_product_and_extraction_limits(self) -> None:
        document = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "contract.json"
            aggregate = copy.deepcopy(document)
            aggregate["archive"]["expanded_max_bytes"] = 150 * 1024 * 1024
            path.write_text(json.dumps(aggregate), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "product budgets exceed expanded archive"):
                self.module.load_contract(path)

            member = copy.deepcopy(document)
            engine = next(
                value for value in member["archive"]["members"] if value["id"] == "engine"
            )
            engine["max_bytes"] = 20_000_000
            path.write_text(json.dumps(member), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "engine product budget exceeds"):
                self.module.load_contract(path)

    def test_generated_rust_is_deterministic_and_current(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "release_contract.rs"
            first = subprocess.run(
                [sys.executable, str(SCRIPT), "generate-rust", "--output", str(output)],
                cwd=REPO,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(first.returncode, 0, first.stderr.decode())
            expected = output.read_bytes()
            second = subprocess.run(
                [sys.executable, str(SCRIPT), "generate-rust", "--output", str(output)],
                cwd=REPO,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(second.returncode, 0, second.stderr.decode())
            self.assertEqual(output.read_bytes(), expected)
            self.assertEqual(GENERATED_RUST.read_bytes(), expected)
            checked = subprocess.run(
                [sys.executable, str(SCRIPT), "generate-rust", "--check"],
                cwd=REPO,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(checked.returncode, 0, checked.stderr.decode())

            typescript_output = Path(directory) / "release-contract.ts"
            generated_typescript = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "generate-typescript",
                    "--output",
                    str(typescript_output),
                ],
                cwd=REPO,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(
                generated_typescript.returncode, 0, generated_typescript.stderr.decode()
            )
            self.assertEqual(GENERATED_TYPESCRIPT.read_bytes(), typescript_output.read_bytes())
            checked_typescript = subprocess.run(
                [sys.executable, str(SCRIPT), "generate-typescript", "--check"],
                cwd=REPO,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(
                checked_typescript.returncode, 0, checked_typescript.stderr.decode()
            )

    def test_engine_product_budget_is_platform_specific(self) -> None:
        contract = self.module.load_contract(CONTRACT_PATH)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            engine = root / "rw"
            with engine.open("wb") as output:
                output.truncate(30_000_000)
            wasm_host = root / "rottweiler-wasm-host"
            wasm_host.write_bytes(b"wasm")
            plugin_host = root / "rottweiler-plugin-host"
            plugin_host.write_bytes(b"plugin")
            tui = root / "rottweiler-tui"
            tui.write_bytes(b"tui")
            native = root / "libopentui.dylib"
            native.write_bytes(b"native")
            self.module.validate_build(
                contract, "darwin-arm64", engine, wasm_host, plugin_host, tui, native
            )
            with self.assertRaisesRegex(ValueError, "product budget is <28000000"):
                self.module.validate_build(
                    contract, "linux-x86_64", engine, wasm_host, plugin_host, tui, native
                )

    def test_stage_release_projects_exact_archive_shape_and_modes(self) -> None:
        contract = self.module.load_contract(CONTRACT_PATH)
        platform_id = "linux-aarch64"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sources: dict[str, Path] = {}
            for member_id in ("engine", "wasm_host", "plugin_host", "tui", "opentui_native"):
                source = root / member_id
                source.write_bytes(member_id.encode("ascii"))
                sources[member_id] = source
            stage = root / contract.archive_root(VERSION, platform_id)
            self.module.stage_release(
                contract,
                stage,
                REPO / "scripts" / "install-release.sh",
                VERSION,
                platform_id,
                sources["engine"],
                sources["wasm_host"],
                sources["plugin_host"],
                sources["tui"],
                sources["opentui_native"],
            )
            platform_contract = contract.platform(platform_id)
            observed = {
                path.relative_to(stage).as_posix()
                for path in stage.rglob("*")
                if path.is_file()
            }
            self.assertEqual(
                observed, {member.path for member in platform_contract.archive_members}
            )
            for member in platform_contract.archive_members:
                self.assertEqual((stage / member.path).stat().st_mode & 0o777, member.mode)
            installer = (stage / "install.sh").read_text(encoding="utf-8")
            self.assertNotRegex(installer, r"@[A-Z][A-Z_]+@")
            archive = Path(f"{stage}.tar.gz")
            packaged = subprocess.run(
                [sys.executable, str(REPO / "scripts" / "package-release.py"), str(stage), str(archive)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(packaged.returncode, 0, packaged.stderr.decode())
            self.module.verify_archive(contract, archive, VERSION, platform_id)

    def test_archive_verifier_accepts_exact_shape_and_rejects_extra_entries(self) -> None:
        contract = self.module.load_contract(CONTRACT_PATH)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / f"rottweiler-{VERSION}-linux-aarch64.tar.gz"
            make_archive(archive, contract, "linux-aarch64")
            self.module.verify_archive(contract, archive, VERSION, "linux-aarch64")

            extra_directory = root / "extra"
            extra_directory.mkdir()
            extra = extra_directory / archive.name
            make_archive(extra, contract, "linux-aarch64", extra=True)
            with self.assertRaisesRegex(ValueError, "exact contract shape"):
                self.module.verify_archive(contract, extra, VERSION, "linux-aarch64")


if __name__ == "__main__":
    unittest.main()
