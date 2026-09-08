"""Verify the exact native candidate or installed release surrounding a soak."""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
from pathlib import Path
import platform as host_platform
import stat
import tarfile

import native_candidate
from release_contract import load_contract, verify_archive, validate_build


@dataclass(frozen=True)
class SoakInputs:
    repo: Path
    rw: Path
    js_host: Path | None
    candidate: Path | None
    release_archive: Path | None
    release_version: str | None

    def verify(self) -> dict:
        if (self.candidate is None) == (self.release_archive is None):
            raise ValueError("soak requires exactly one candidate or release archive")
        rw = self.rw.resolve(strict=True)
        js_host = (self.js_host or rw.with_name("rottweiler-js-host")).resolve(strict=True)
        if self.candidate is not None:
            if self.release_version is not None:
                raise ValueError("release version requires a release archive")
            candidate = self.candidate.resolve(strict=True)
            receipt = native_candidate.verify(candidate, self.repo)
            components = receipt["components"]
            for selected, role in ((rw, "engine"), (js_host, "js_host")):
                if selected != (candidate / components[role]["path"]).resolve(strict=True):
                    raise ValueError(f"soak {role} differs from verified candidate")
            return {"kind": "native_candidate", "candidate": str(candidate),
                    "receipt_sha256": native_candidate.hash_file(candidate / "build.json"),
                    "identity": receipt["identity"], "components": components,
                    "rw": str(rw), "js_host": str(js_host)}
        if self.release_version is None:
            raise ValueError("release archive requires its exact release version")
        contract = load_contract(self.repo / "contracts/release-contract.json")
        platform = contract.resolve_platform(host_platform.system(), host_platform.machine())
        archive = self.release_archive.resolve(strict=True)
        verify_archive(contract, archive, self.release_version, platform.id)
        release_root = contract.archive_root(self.release_version, platform.id)
        installed_bin = rw.parent
        if js_host != installed_bin / "rottweiler-js-host":
            raise ValueError("installed release cannot select a different JS host")
        members = [member for member in platform.archive_members if member.path.startswith("bin/")]
        expected = {Path(member.path).name for member in members}
        if {path.name for path in installed_bin.iterdir()} != expected:
            raise ValueError("installed soak bin inventory differs from release contract")
        actual = {}
        with tarfile.open(archive, "r:gz") as bundle:
            for member in members:
                path = installed_bin / Path(member.path).name
                metadata = path.lstat()
                if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
                        or stat.S_IMODE(metadata.st_mode) != member.mode):
                    raise ValueError("installed soak member has invalid identity/mode")
                info = bundle.getmember(f"{release_root}/{member.path}")
                stream = bundle.extractfile(info)
                if stream is None:
                    raise ValueError("release member is not a regular file")
                with stream:
                    digest = hashlib.file_digest(stream, "sha256").hexdigest()
                if metadata.st_size != info.size or native_candidate.hash_file(path) != digest:
                    raise ValueError("installed soak member differs from release archive")
                actual[member.id] = {"sha256": digest, "bytes": info.size, "path": str(path)}
        validate_build(contract, platform.id, rw, Path(actual["wasm_host"]["path"]),
                       js_host, Path(actual["opentui_native"]["path"]))
        return {"kind": "installed_release_archive", "source": native_candidate.source_identity(self.repo),
                "platform": platform.id, "version": self.release_version,
                "archive": str(archive), "archive_sha256": native_candidate.hash_file(archive),
                "components": actual, "rw": str(rw), "js_host": str(js_host)}


def verify_unchanged(inputs: SoakInputs, before: dict) -> dict:
    after = inputs.verify()
    if after != before:
        raise ValueError("soak source or native artifacts changed during the run")
    return after
