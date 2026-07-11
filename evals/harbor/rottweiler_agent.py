"""Harbor installed-agent adapter for a locally built Rottweiler release."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shlex
import tarfile
import tempfile
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths


_CREDENTIAL_ENV = (
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GITHUB_TOKEN",
    "GITHUB_COPILOT_TOKEN",
)


class Rottweiler(BaseInstalledAgent):
    """Run Rottweiler's headless CLI inside a Harbor task container."""

    @staticmethod
    def name() -> str:
        return "rottweiler"

    def get_version_command(self) -> str | None:
        return "rw --version"

    @staticmethod
    def _validate_archive(archive: Path) -> None:
        total = 0
        rw_members = 0
        with tarfile.open(archive, "r:gz") as bundle:
            members = bundle.getmembers()
            if not 1 <= len(members) <= 16:
                raise RuntimeError("release archive has an invalid entry count")
            for member in members:
                path = Path(member.name)
                if path.is_absolute() or ".." in path.parts or not (member.isdir() or member.isfile()):
                    raise RuntimeError("release archive contains an unsafe entry")
                total += member.size
                if total > 150 * 1024 * 1024:
                    raise RuntimeError("release archive exceeds the expanded-size limit")
                if member.isfile() and member.name.endswith("/bin/rw"):
                    rw_members += 1
        if rw_members != 1:
            raise RuntimeError("release archive must contain exactly one bin/rw")

    async def install(self, environment: BaseEnvironment) -> None:
        archive_value = os.environ.get("ROTTWEILER_RELEASE_ARCHIVE")
        if not archive_value:
            raise RuntimeError("ROTTWEILER_RELEASE_ARCHIVE is required")
        archive = Path(archive_value).expanduser().resolve()
        if not archive.is_file():
            raise RuntimeError(f"release archive is unavailable: {archive}")
        self._validate_archive(archive)
        digest_builder = hashlib.sha256()
        with archive.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest_builder.update(chunk)
        digest = digest_builder.hexdigest()
        remote = "/tmp/rottweiler-release.tar.gz"
        await environment.upload_file(archive, remote)
        await self.exec_as_root(
            environment,
            command=(
                "set -euo pipefail; "
                f"printf '%s  %s\\n' {shlex.quote(digest)} {shlex.quote(remote)} "
                "| sha256sum -c -; "
                "rm -rf /installed-agent/rottweiler; "
                "mkdir -p /installed-agent/rottweiler; "
                f"tar -xzf {shlex.quote(remote)} "
                "-C /installed-agent/rottweiler --strip-components=1; "
                "install -m 0755 /installed-agent/rottweiler/bin/rw /usr/local/bin/rw; "
                f"rm -f {shlex.quote(remote)}; rw --version"
            ),
        )

    def _model_config(self) -> str:
        if not self.model_name or not re.fullmatch(
            r"[a-z0-9][a-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._:/-]*", self.model_name
        ):
            raise ValueError("model must be a pinned provider/model identifier")
        return (
            "[models]\n"
            'default = "benchmark"\n'
            "[models.aliases]\n"
            f"benchmark = [{json.dumps(self.model_name)}]\n"
        )

    async def _upload_private_credentials(self, environment: BaseEnvironment) -> str:
        values = {key: os.environ[key] for key in _CREDENTIAL_ENV if os.environ.get(key)}
        if not values:
            raise RuntimeError("no supported provider credential is available for the live eval")
        descriptor, local_name = tempfile.mkstemp(prefix="rottweiler-eval-credentials-")
        local = Path(local_name)
        remote = "/tmp/rottweiler-eval-credentials"
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                for key, value in values.items():
                    handle.write(f"export {key}={shlex.quote(value)}\n")
            local.chmod(0o600)
            await environment.upload_file(local, remote)
        finally:
            local.unlink(missing_ok=True)
        identity = await environment.exec("id -u; id -g", timeout_sec=10)
        parts = (identity.stdout or "").splitlines()
        if identity.return_code != 0 or len(parts) != 2 or not all(part.isdigit() for part in parts):
            raise RuntimeError("could not determine Harbor agent identity")
        await self.exec_as_root(
            environment,
            command=(
                f"chown {parts[0]}:{parts[1]} {shlex.quote(remote)}; "
                f"chmod 0600 {shlex.quote(remote)}"
            ),
        )
        return remote

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        del context
        config = self.logs_dir / "benchmark-config.toml"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(self._model_config(), encoding="utf-8")
        remote_config = "/tmp/rottweiler-config.toml"
        await environment.upload_file(config, remote_config)
        await self.exec_as_root(
            environment, command=f"chmod 0644 {shlex.quote(remote_config)}"
        )
        credential_file = await self._upload_private_credentials(environment)
        agent_log = (EnvironmentPaths.agent_dir / "rottweiler.jsonl").as_posix()
        stats_log = (EnvironmentPaths.agent_dir / "rottweiler-stats.json").as_posix()
        await self.exec_as_agent(
            environment,
            command=(
                "set -euo pipefail; "
                "mkdir -p \"$HOME/.rottweiler\" "
                f"{shlex.quote(EnvironmentPaths.agent_dir.as_posix())}; "
                f"install -m 0600 {shlex.quote(remote_config)} "
                '"$HOME/.rottweiler/config.toml"; '
                f". {shlex.quote(credential_file)}; "
                f"rm -f {shlex.quote(credential_file)} {shlex.quote(remote_config)}; "
                "export ROTTWEILER_CREDENTIAL_BACKEND=file; "
                f"rw -p {shlex.quote(instruction)} --model benchmark "
                "--permission-mode yolo --max-turns 128 "
                f"--output-format stream-json 2>&1 </dev/null | tee {shlex.quote(agent_log)}"
            ),
            timeout_sec=3600,
        )
        await self.exec_as_agent(
            environment,
            command=(
                "export ROTTWEILER_CREDENTIAL_BACKEND=file; "
                f"rw stats --json > {shlex.quote(stats_log)}"
            ),
            timeout_sec=60,
        )
