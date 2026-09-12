#!/usr/bin/env python3
"""Join a verified native engine, prepared SDK extension and compiled real TUI.

This is a functional acceptance run, not a latency sample. No command compiles.
The private relay forwards actual protocol bytes; its separate control socket
withholds one bounded artifact response or disconnects the actual connection.
"""
from __future__ import annotations
import argparse
import asyncio
import json
import os
from pathlib import Path
import signal
import time

import native_candidate
import rich_fixture_inputs as inputs
from perf_process import require_sample_settlement
from perf_process_wait import signal_owned_group
from release_contract import load_contract
from rich_impairment import ImpairmentRelay, header
from rich_native_process import NativeProcess

REPO = Path(__file__).resolve().parents[1]
MAX_REPORT = 1024 * 1024


def bounded_json(path: Path, limit=MAX_REPORT):
    with path.open("rb") as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("native rich evidence exceeds bound")
    return json.loads(raw)


def write_json(path: Path, value):
    encoded = json.dumps(value, sort_keys=True).encode() + b"\n"
    if len(encoded) > MAX_REPORT:
        raise ValueError("native rich report exceeds bound")
    with path.open("xb") as stream:
        stream.write(encoded)
    path.chmod(0o600)


async def ready(engine, socket: Path, token_file: Path):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        engine.drain()
        if engine.owner.observe_exit() is not None:
            raise RuntimeError("native engine exited before authenticated health")
        if socket.exists() and token_file.exists():
            with token_file.open("rb") as stream:
                token = stream.read(1025).strip()
            if not token or len(token) > 1024 or any(byte <= 32 or byte >= 127 for byte in token):
                raise ValueError("invalid private bootstrap authority")
            writer = None
            try:
                async with asyncio.timeout(1):
                    reader, writer = await asyncio.open_unix_connection(socket, limit=16384)
                    writer.write(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer "
                                 + token + b"\r\nContent-Length: 0\r\n\r\n")
                    await writer.drain()
                    raw, fields = await header(reader)
                    count = int(fields.get("content-length", "-1"))
                    if not 0 <= count <= 8192:
                        raise ValueError("health response lacks bounded framing")
                    await reader.readexactly(count)
                    if raw.split(b" ", 2)[1] == b"200":
                        return
            except (ConnectionError, OSError, TimeoutError, asyncio.IncompleteReadError):
                pass
            finally:
                if writer is not None:
                    writer.close()
                    await writer.wait_closed()
        await asyncio.sleep(.02)
    raise TimeoutError("native engine health deadline elapsed")


def canonical_proof(home: Path):
    path = home / "sessions/rich-native/journal/active.jsonl"
    with path.open("rb") as stream:
        raw = stream.read(MAX_REPORT + 1)
    if len(raw) > MAX_REPORT or not raw.endswith(b"\n"):
        raise ValueError("canonical rich journal exceeds bounded complete-record contract")
    lines = raw.splitlines()
    if len(lines) > 512:
        raise ValueError("canonical rich journal exceeds record count")
    events = [json.loads(line)["event"] for line in lines]
    started = [event for event in events if event.get("type") == "tool_call_started"
               and event.get("name") == "rich_artifact"]
    if len(started) != 1:
        raise ValueError("native SDK did not start exactly one declared rich tool")
    tools = [event for event in events if event.get("type") == "tool_call_finished"
             and event.get("invocation_id") == started[0]["invocation_id"]]
    if len(tools) != 1 or tools[0]["is_error"]:
        raise ValueError("native SDK did not commit exactly one successful rich artifact")
    commits = [event for event in events if event.get("type") == "extension_state_committed"
               and event.get("plugin_id") == "rich-workflow"]
    values = [mutation["value"] for event in commits for mutation in event["transaction"]["mutations"]
              if mutation.get("action") == "set" and mutation.get("key") == "advances"]
    if values != [0, 1, 2]:
        raise ValueError("native SDK actions did not commit their exact canonical state changes")
    return {"journal_sha256": native_candidate.hash_file(path), "journal_bytes": len(raw),
            "events": len(events), "tool_completions": len(tools), "advances": values}


async def run(candidate: Path, receipt: Path, output: Path):
    before = inputs.verify(candidate, receipt, REPO)
    if native_candidate.source_identity(REPO) != before["candidate"]["identity"]["source"]:
        raise ValueError("native rich harness source differs from candidate")
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    output = output.resolve(strict=True)
    # Short explicit output paths also keep all Unix authority names portable.
    if len(os.fsencode(output / "control.sock")) > 100:
        raise ValueError("native rich output path exceeds Unix socket authority limit")
    home, workspace, temporary = (output / name for name in ("home", "workspace", "tmp"))
    for path in (home, workspace, temporary, home / ".rottweiler"):
        path.mkdir(mode=0o700)
    package = receipt.parent.resolve(strict=True)
    prepared = before["prepared"]
    config = "[[plugins]]\nname = \"rich-workflow\"\n" + "\n".join(
        f"{key} = {json.dumps(value)}" for key, value in {
            "argv": [prepared["bun"]["path"], str(package / "plugin.js")], "cwd": str(package),
            "manifest": str(package / "manifest.json"), "allowed_domains": [],
        }.items()) + "\n"
    (home / ".rottweiler/plugins.toml").write_text(config)
    script = output / "provider.json"
    write_json(script, [[{"type": "text_delta", "text": "Unexpected provider invocation"},
                         {"type": "finished", "reason": "stop"}]])
    environment = {"HOME": str(home), "ROTTWEILER_HOME": str(home), "TMPDIR": str(temporary),
                   "PATH": str(Path(prepared["bun"]["path"]).parent) + ":/usr/bin:/bin",
                   "TERM": "xterm-256color", "LANG": "en_US.UTF-8", "RUST_LOG": "warn"}
    parts = before["candidate"]["components"]
    engine_path = str(candidate / parts["engine"]["path"])
    host_path = str(candidate / parts["js_host"]["path"])
    engine = ui = relay = None
    result = {"schema_version": 1, "kind": "native_third_party_rich_functional", "inputs": before,
              "passed": False, "errors": []}
    try:
        approval = NativeProcess([engine_path, "plugin", "approve", "rich-workflow"], cwd=workspace,
                                 env=environment, log=output / "approval.log", interactive=True)
        try:
            await approval.finish(30, approve=True)
        finally:
            approval.close()
        socket, token = output / "engine.sock", output / "engine.token"
        engine = NativeProcess([engine_path, "serve", "--detach", "--session", "rich-native", "--workspace", str(workspace),
                                "--socket", str(socket), "--token-file", str(token),
                                "--in-memory-replay-script", str(script)], cwd=workspace, env=environment,
                               log=output / "engine.log")
        result["engine_pid"] = engine.owner.process.pid
        await ready(engine, socket, token)
        relay = await ImpairmentRelay(socket, output / "client.sock", output / "control.sock").start()
        write_json(output / "native-rich-input.json", {"socketPath": str(relay.socket), "bootstrapTokenFile": str(token),
                   "sessionId": "rich-native", "impairmentSocketPath": str(relay.control)})
        role = load_contract(REPO / "contracts/release-contract.json").js_host_roles["tui"]
        ui = NativeProcess([host_path, role], cwd=workspace,
                           env={**environment, "ROTTWEILER_CLIENT_NATIVE_RICH_DIRECTORY": str(output)},
                           log=output / "ui.log")
        result["ui_pid"] = ui.owner.process.pid
        await ui.finish(120, companion=engine)
        observed = bounded_json(output / "native-rich.json")
        if observed.get("passed") is not True or observed.get("finalAllocationBytes") != 0:
            raise ValueError("compiled native rich UI did not prove retirement")
        result["ui"] = observed
        result["canonical"] = canonical_proof(home)
    except BaseException as error:
        result["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        # Each owner closes even if a previous close fails. A failed proof never passes.
        try:
            if relay is not None:
                await relay.close()
        except BaseException as error:
            result["errors"].append(f"relay retirement: {error}")
        if relay:
            result["relay"] = {**relay.status(), "receipts": relay.receipts,
                               "remaining_bytes": relay.held_bytes, "remaining_jobs": len(relay.tasks)}
        for child in (ui, engine):
            if child is None:
                continue
            try:
                if child is engine and child.owner.observe_exit() is None:
                    signal_owned_group(child.owner.process.pid, signal.SIGINT, timeout=1)
                    await child.finish(10)
            except BaseException as error:
                result["errors"].append(f"engine cooperative retirement: {error}")
            finally:
                try:
                    child.close()
                except BaseException as error:
                    result["errors"].append(f"process retirement: {error}")
        try:
            require_sample_settlement()
            if inputs.verify(candidate, receipt, REPO) != before or native_candidate.source_identity(REPO) != before["candidate"]["identity"]["source"]:
                raise ValueError("native rich source/input identity changed")
        except BaseException as error:
            result["errors"].append(f"after-input verification: {error}")
        result["passed"] = not result["errors"]
        write_json(output / "result.json", result)
    if not result["passed"]:
        raise RuntimeError(f"native rich acceptance failed; retained evidence: {output}")
    return output / "result.json"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--fixture-receipt", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    print(asyncio.run(run(args.candidate.resolve(strict=True), args.fixture_receipt.resolve(strict=True), args.output)))
