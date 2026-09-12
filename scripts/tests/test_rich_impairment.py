"""Relay unit oracles; upstream fixtures here do not qualify the native workflow."""
import asyncio
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rich_impairment import ImpairmentRelay, MAX_CONNECTIONS, MAX_RESPONSE


class RelayTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="rw-rich-relay-", dir="/tmp")
        root = Path(self.directory.name)
        self.upstream = root / "engine.sock"
        self.received = []
        self.payload = b'{"text":"actual bounded engine bytes"}'
        self.upstream_tasks = set()
        self.upstream_writers = set()
        self.server = await asyncio.start_unix_server(self.engine, path=self.upstream)
        self.relay = await ImpairmentRelay(self.upstream, root / "relay.sock", root / "control.sock").start()
        self.clients = []

    async def asyncTearDown(self):
        for writer in self.clients:
            writer.close()
            await writer.wait_closed()
        await self.relay.close()
        self.server.close()
        await self.server.wait_closed()
        for writer in self.upstream_writers:
            writer.close()
        for task in self.upstream_tasks:
            task.cancel()
        await asyncio.gather(*self.upstream_tasks, return_exceptions=True)
        self.directory.cleanup()

    async def engine(self, reader, writer):
        task = asyncio.current_task()
        self.upstream_tasks.add(task)
        self.upstream_writers.add(writer)
        try:
            raw = await reader.readuntil(b"\r\n\r\n")
            count = int(next(line.split(b":", 1)[1] for line in raw.split(b"\r\n") if line.lower().startswith(b"content-length:")))
            body = await reader.readexactly(count)
            self.received.append(raw + body)
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(self.payload)).encode() + b"\r\n\r\n" + self.payload)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()
            self.upstream_tasks.discard(task)
            self.upstream_writers.discard(writer)

    async def connect(self, body=None, framing=None):
        reader, writer = await asyncio.open_unix_connection(self.relay.socket)
        self.clients.append(writer)
        body = json.dumps(body or {}).encode()
        raw = framing or b"POST /command HTTP/1.1\r\nConnection: close\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n"
        writer.write(raw + body)
        await writer.drain()
        return reader, writer, raw + body

    async def until(self, predicate):
        async with asyncio.timeout(2):
            while not predicate():
                await asyncio.sleep(.001)

    def arm(self):
        self.relay.armed = True
        return {"type": "read_transcript_content", "read": {"offset": 0,
                "source": {"selector": {"type": "tool_output"}, "sequence": 7}}}

    async def test_exact_response_and_request_bytes(self):
        reader, _, request = await self.connect()
        result = await asyncio.wait_for(reader.read(), 2)
        self.assertEqual(self.received, [request])
        self.assertEqual(result.split(b"\r\n\r\n", 1)[1], self.payload)

    async def test_held_release_keeps_exact_hash_and_bytes(self):
        reader, _, _ = await self.connect(self.arm())
        await self.until(lambda: self.relay.held == 1)
        self.assertEqual(self.relay.receipts[0]["sha256"], hashlib.sha256(self.payload).hexdigest())
        self.relay.release.set()
        self.assertTrue((await reader.read()).endswith(self.payload))
        await self.until(lambda: self.relay.held_bytes == 0)
        self.assertTrue(self.relay.receipts[0]["delivered"])

    async def test_caller_loss_retires_held_body_without_release(self):
        _, writer, _ = await self.connect(self.arm())
        await self.until(lambda: self.relay.held == 1)
        writer.close()
        await writer.wait_closed()
        await self.until(lambda: not self.relay.tasks)
        self.assertEqual(self.relay.held_bytes, 0)
        self.assertFalse(self.relay.receipts[0]["delivered"])

    async def test_oversize_response_rejected_before_body_retention(self):
        self.payload = b"x" * (MAX_RESPONSE + 1)
        reader, _, _ = await self.connect(self.arm())
        self.assertEqual(await reader.read(), b"")
        self.assertEqual(self.relay.peak, 0)
        self.assertEqual(self.relay.receipts, [])

    async def test_keepalive_and_oversize_request_are_rejected_before_upstream(self):
        for header in (b"Content-Length: 2", b"Connection: close\r\nContent-Length: 9999999"):
            reader, _, _ = await self.connect(framing=b"POST / HTTP/1.1\r\n" + header + b"\r\n\r\n")
            self.assertEqual(await reader.read(), b"")
        self.assertEqual(self.received, [])

    async def test_control_disconnect_drops_real_connection_without_fabricated_reply(self):
        reader, _, _ = await self.connect(self.arm())
        await self.until(lambda: self.relay.held == 1)
        control, writer = await asyncio.open_unix_connection(self.relay.control)
        self.clients.append(writer)
        writer.write(b"POST /disconnect HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
        await writer.drain()
        self.assertIn(b"200 OK", await control.read())
        self.assertEqual(await reader.read(), b"")
        await self.until(lambda: not self.relay.tasks)
        self.assertTrue(self.relay.offline)
        self.assertEqual(self.relay.held_bytes, 0)
        self.assertFalse(self.relay.receipts[0]["delivered"])

    async def test_failed_second_listener_closes_first_listener(self):
        root = Path(self.directory.name)
        invalid = root / "occupied"
        invalid.write_text("not a socket")
        relay = ImpairmentRelay(self.upstream, root / "extra.sock", invalid)
        with self.assertRaises(OSError):
            await relay.start()
        self.assertTrue(all(not server.is_serving() for server in relay.servers))
        self.assertFalse(relay.tasks)

    async def test_saturated_open_headers_are_finite_and_joined(self):
        for _ in range(MAX_CONNECTIONS):
            _, writer = await asyncio.open_unix_connection(self.relay.socket)
            self.clients.append(writer)
        await self.until(lambda: len(self.relay.tasks) == MAX_CONNECTIONS)
        reader, writer = await asyncio.open_unix_connection(self.relay.socket)
        self.clients.append(writer)
        self.assertEqual(await reader.read(), b"")
        await self.relay.close()
        self.assertFalse(self.relay.tasks)
        self.assertFalse(self.relay.writers)


if __name__ == "__main__":
    unittest.main()
