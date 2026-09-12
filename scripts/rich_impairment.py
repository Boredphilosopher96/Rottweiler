"""Bounded Unix impairment for a fixture client explicitly using Connection: close.

Requests including that header and all engine response bytes are forwarded unchanged.
Only the separate private control socket emits synthetic fixture status.
"""
from __future__ import annotations
import asyncio
import contextlib
import hashlib
import json
from pathlib import Path

MAX_CONNECTIONS = 12
MAX_REQUEST = 512 * 1024
MAX_RESPONSE = 256 * 1024
HEADER_LIMIT = 16 * 1024
HOLD_SECONDS = 10


async def header(reader: asyncio.StreamReader) -> tuple[bytes, dict[str, str]]:
    raw = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 30)
    if len(raw) > HEADER_LIMIT:
        raise ValueError("relay header exceeds bound")
    fields = {}
    for line in raw.split(b"\r\n")[1:-2]:
        key, value = line.decode("latin1").split(":", 1)
        key = key.lower()
        if key in fields:
            raise ValueError("relay rejects duplicate headers")
        fields[key] = value.strip()
    return raw, fields


class ImpairmentRelay:
    def __init__(self, upstream: Path, socket: Path, control: Path):
        self.upstream, self.socket, self.control = upstream, socket, control
        self.tasks: set[asyncio.Task] = set()
        self.writers: set[asyncio.StreamWriter] = set()
        self.armed = False
        self.offline = False
        self.held = 0
        self.held_bytes = 0
        self.peak = 0
        self.failures = 0
        self.receipts = []
        self.release = asyncio.Event()
        self.servers = []

    async def start(self):
        try:
            self.servers.append(await asyncio.start_unix_server(self.accept, path=self.socket, limit=HEADER_LIMIT))
            self.servers.append(await asyncio.start_unix_server(self.control_request, path=self.control, limit=HEADER_LIMIT))
            self.socket.chmod(0o600)
            self.control.chmod(0o600)
            return self
        except BaseException:
            await self.close()
            raise

    async def close(self):
        self.offline = True
        for server in self.servers:
            server.close()
        for server in self.servers:
            await server.wait_closed()
        self.release.set()
        for writer in list(self.writers):
            writer.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)
        if self.held or self.held_bytes or self.writers or self.tasks:
            raise RuntimeError("relay retained work after joined close")

    def status(self):
        return {"held": self.held, "selected": len(self.receipts),
                "completed": sum(item["delivered"] for item in self.receipts),
                "peakHeldBytes": self.peak, "failures": self.failures}

    async def control_request(self, reader, writer):
        task = asyncio.current_task()
        if len(self.tasks) >= MAX_CONNECTIONS + 2:
            writer.close()
            await writer.wait_closed()
            return
        self.tasks.add(task)
        self.writers.add(writer)
        try:
            raw, fields = await header(reader)
            method, path, _ = raw.split(b"\r\n", 1)[0].decode().split(" ")
            count = int(fields.get("content-length", "0"))
            if not 0 <= count <= 256 or "transfer-encoding" in fields:
                raise ValueError("invalid relay control body")
            body = await asyncio.wait_for(reader.readexactly(count), 5)
            if (method, path) == ("POST", "/arm") and json.loads(body) == {"mode": "hold"}:
                if self.armed or self.held:
                    raise ValueError("relay is already armed")
                self.release.clear()
                self.armed = True
            elif (method, path) == ("POST", "/release") and not body:
                self.release.set()
            elif (method, path) == ("POST", "/disconnect") and not body:
                self.offline = True
                self.release.set()
                for connection in list(self.writers):
                    if connection is not writer:
                        connection.close()
            elif (method, path) == ("POST", "/resume") and not body:
                self.offline = False
            elif (method, path) != ("GET", "/status") or body:
                raise ValueError("unsupported relay control")
            encoded = json.dumps(self.status(), separators=(",", ":")).encode()
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: "
                         + str(len(encoded)).encode() + b"\r\n\r\n" + encoded)
            await writer.drain()
        except (ValueError, RecursionError, OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
            self.failures += 1
        finally:
            writer.close()
            with contextlib.suppress(OSError):
                await writer.wait_closed()
            self.writers.discard(writer)
            self.tasks.discard(task)

    async def accept(self, reader, writer):
        task = asyncio.current_task()
        if self.offline or len(self.tasks) >= MAX_CONNECTIONS:
            writer.close()
            await writer.wait_closed()
            return
        self.tasks.add(task)
        self.writers.add(writer)
        remote = None
        try:
            raw, fields = await header(reader)
            count = int(fields.get("content-length", "0"))
            if (not 0 <= count <= MAX_REQUEST or "transfer-encoding" in fields
                    or fields.get("connection", "").lower() != "close"):
                raise ValueError("relay request framing exceeds bound")
            body = await asyncio.wait_for(reader.readexactly(count), 30)
            request = json.loads(body) if body else None
            read = request.get("read") if isinstance(request, dict) else None
            source = read.get("source") if isinstance(read, dict) else None
            selector = source.get("selector") if isinstance(source, dict) else None
            selected = (self.armed and isinstance(request, dict)
                        and request.get("type") == "read_transcript_content"
                        and isinstance(selector, dict) and selector.get("type") == "tool_output")
            if selected and (len(json.dumps(source).encode()) > 1024
                             or type(read.get("offset")) is not int or read["offset"] < 0):
                raise ValueError("relay receipt source exceeds bound")
            if selected:
                self.armed = False
            incoming, remote = await asyncio.wait_for(asyncio.open_unix_connection(self.upstream, limit=HEADER_LIMIT), 5)
            self.writers.add(remote)
            remote.write(raw)
            remote.write(body)
            await remote.drain()
            del body, raw
            if selected:
                await self.until_disconnect(reader, self.hold_response(incoming, writer, request))
            else:
                await self.until_disconnect(reader, self.forward(incoming, writer))
        except (ValueError, RecursionError, OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
            self.failures += 1
        finally:
            for stream in (remote, writer):
                if stream is not None:
                    stream.close()
                    with contextlib.suppress(OSError):
                        await stream.wait_closed()
                    self.writers.discard(stream)
            self.tasks.discard(task)

    async def until_disconnect(self, caller, operation):
        async def disconnected():
            if await caller.read(1):
                raise ValueError("relay rejects pipelined requests")
        tasks = [asyncio.create_task(operation), asyncio.create_task(disconnected())]
        try:
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in done:
                task.result()
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def forward(self, reader, writer):
        while block := await asyncio.wait_for(reader.read(64 * 1024), 30):
            writer.write(block)
            await writer.drain()

    async def hold_response(self, incoming, writer, request):
        raw, fields = await header(incoming)
        if "transfer-encoding" in fields or "content-length" not in fields:
            raise ValueError("held engine response requires bounded explicit framing")
        count = int(fields["content-length"])
        if not 0 <= count <= MAX_RESPONSE or len(self.receipts) >= 8:
            raise ValueError("held engine response exceeds receipt/byte bounds")
        body = await asyncio.wait_for(incoming.readexactly(count), 5)
        record = {"source": request["read"]["source"], "offset": request["read"]["offset"],
                  "bytes": count, "sha256": hashlib.sha256(body).hexdigest(), "delivered": False}
        self.receipts.append(record)
        self.held += 1
        self.held_bytes += count + len(raw)
        self.peak = max(self.peak, self.held_bytes)
        try:
            await asyncio.wait_for(self.release.wait(), HOLD_SECONDS)
            if not self.offline and not writer.is_closing():
                writer.write(raw)
                writer.write(body)
                await writer.drain()
                record["delivered"] = True
        finally:
            self.held -= 1
            self.held_bytes -= count + len(raw)
