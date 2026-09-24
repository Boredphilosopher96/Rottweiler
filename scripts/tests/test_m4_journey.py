"""Validate native provider sequencing without injecting any engine state."""
import http.client
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from m4_journey_gate import (
    CHILDREN, CHILD_TASKS, EDIT, OBSERVE, RESUME, SUMMARY, new_server, route_request,
)


def request(prompt, returned=()):
    return {"messages": [{"role": "user", "content": prompt},
                         *({"role": "tool", "tool_call_id": call, "content": "ok"} for call in returned)]}


class NativeJourneyTests(unittest.TestCase):
    def test_edit_test_sequence_uses_returned_tool_ids(self):
        action, calls = route_request(request(EDIT + " journey-edit journey-test"))
        self.assertEqual(action, "tools")
        self.assertEqual(calls[0]["function"]["name"], "edit")
        self.assertEqual(json.loads(calls[0]["function"]["arguments"]),
                         {"path": "journey.txt", "old": "before", "new": "after"})
        _, calls = route_request(request(EDIT, ["journey-edit"]))
        self.assertEqual(calls[0]["function"]["name"], "bash")
        self.assertEqual(route_request(request(EDIT, ["journey-edit", "journey-test"]))[0], "text")

    def test_background_children_are_independent_of_parent_and_each_other(self):
        _, calls = route_request(request(CHILDREN))
        self.assertEqual([call["index"] for call in calls], [0, 1])
        self.assertEqual(len({call["id"] for call in calls}), 2)
        for index, task in enumerate(CHILD_TASKS):
            body = request(CHILDREN)
            body["messages"].append({"role": "user", "content": task})
            self.assertEqual(route_request(body), ("child", index))
        self.assertEqual(route_request(request(CHILDREN, ["journey-child-1", "journey-child-2"])),
                         ("text", "NATIVE_PARENT_CONTINUED"))

    def test_missing_completion_or_summary_context_fails_closed(self):
        for prompt in (OBSERVE, RESUME):
            with self.assertRaises(ValueError):
                route_request(request(prompt))
        self.assertEqual(route_request(request(OBSERVE + " NATIVE_CHILD_RESULT_1 NATIVE_CHILD_RESULT_2"))[0], "text")
        self.assertEqual(route_request(request(RESUME + " " + SUMMARY))[0], "text")

    def test_title_is_distinct_from_compaction(self):
        body = request(EDIT)
        body["tool_choice"] = "none"
        with self.assertRaises(ValueError):
            route_request(body)
        body["messages"].insert(0, {"role": "system", "content": "Name this coding session"})
        self.assertNotEqual(route_request(body)[1], SUMMARY)
        body["messages"] = [{"role": "user", "content": "Create a hand-off summary"}]
        self.assertEqual(route_request(body)[1], SUMMARY)

    def test_loopback_provider_emits_real_openai_tool_frames_and_retains_requests(self):
        server, thread = new_server()
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
        body = request(CHILDREN)
        try:
            connection.request("POST", "/v1/chat/completions", json.dumps(body), {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read().decode()
            self.assertEqual(response.status, 200)
            self.assertIn('"finish_reason": "tool_calls"', data)
            self.assertIn("spawn_agent", data)
            self.assertTrue(data.endswith("data: [DONE]\n\n"))
            self.assertEqual(server.requests, [body])
            self.assertEqual(server.errors, [])
        finally:
            connection.close()
            server.release.set()
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
