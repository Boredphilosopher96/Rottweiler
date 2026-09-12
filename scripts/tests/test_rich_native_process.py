"""Real child cancellation and prompt ownership, without compiling native products."""
import asyncio
import importlib.util
import json
from pathlib import Path
import os
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rich_native_process import NativeProcess

spec = importlib.util.spec_from_file_location("native_rich", Path(__file__).resolve().parents[1] / "native-rich-extension.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class NativeProcessTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="rw-rich-process-", dir="/tmp")
        self.root = Path(self.directory.name)
        self.child = None

    async def asyncTearDown(self):
        if self.child is not None:
            self.child.close()
        self.directory.cleanup()

    def start(self, source, interactive=False):
        self.child = NativeProcess([sys.executable, "-c", source], cwd=self.root, env=dict(os.environ),
                                   log=self.root / "child.log", interactive=interactive)
        return self.child

    async def test_real_approval_requires_observed_prompt(self):
        child = self.start('import sys; print("Approve this exact plugin identity? [y/N]", file=sys.stderr, flush=True); assert input()=="yes"', True)
        await child.finish(2, approve=True)
        child.close()
        self.assertTrue(child.owner.finished)

    async def test_wrong_prompt_does_not_receive_approval(self):
        child = self.start('print("Unrelated command"); import time; time.sleep(30)', True)
        with self.assertRaises(TimeoutError):
            await child.finish(.1, approve=True)
        child.close()
        self.assertTrue(child.owner.finished)

    async def test_cancelled_observer_keeps_owner_until_explicit_settlement(self):
        child = self.start('import time; time.sleep(30)')
        observer = asyncio.create_task(child.finish(30))
        await asyncio.sleep(.02)
        observer.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await observer
        self.assertIsNone(child.owner.observe_exit())
        child.close()
        self.assertTrue(child.owner.finished)

    def test_canonical_proof_correlates_exact_invocation_and_private_namespace(self):
        path = self.root / "sessions/rich-native/journal/active.jsonl"
        path.parent.mkdir(parents=True)
        records = [
            {"type": "tool_call_started", "name": "rich_artifact", "invocation_id": "actual"},
            {"type": "tool_call_finished", "invocation_id": "actual", "is_error": False},
            *[{"type": "extension_state_committed", "plugin_id": "rich-workflow", "transaction": {
                "mutations": [{"action": "set", "key": "advances", "value": value}]}} for value in (0, 1, 2)],
        ]
        def write():
            path.write_text("".join(json.dumps({"event": record}) + "\n" for record in records))
        write()
        self.assertEqual(module.canonical_proof(self.root)["advances"], [0, 1, 2])
        records[1]["invocation_id"] = "wrong"
        write()
        with self.assertRaisesRegex(ValueError, "successful rich artifact"):
            module.canonical_proof(self.root)


if __name__ == "__main__":
    unittest.main()
