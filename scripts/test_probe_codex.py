"""Run with python -m unittest discover -s scripts -p test_probe_codex.py."""
import importlib.util
import io
import json
from pathlib import Path
import sys
import time
import unittest


class ProbeTests(unittest.TestCase):
    def setUp(self):
        path = Path(__file__).with_name("probe-codex.py")
        self.assertTrue(path.exists(), "bounded Codex probe must exist")
        spec = importlib.util.spec_from_file_location("probe_codex", path)
        self.probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.probe)

    def test_redacts_secrets_in_nested_payloads_and_error_text(self):
        output = io.StringIO()
        record = self.probe.Recorder(output, ["sentinel-secret"])
        record.write("recv", {"error": {"message": "key sentinel-secret rejected"},
                              "access_token": "unknown-token", "safe": "hello"})
        text = output.getvalue()
        self.assertNotIn("sentinel-secret", text)
        self.assertNotIn("unknown-token", text)
        self.assertEqual(json.loads(text)["message"]["safe"], "hello")

    def test_request_waits_for_its_reply_and_keeps_notifications(self):
        server = """import sys,json
for line in sys.stdin:
    request=json.loads(line)
    print(json.dumps({'method':'test/event','params':{'text':'sentinel-secret'}}),flush=True)
    print(json.dumps({'id':request['id'],'result':{'ok':True}}),flush=True)
"""
        output = io.StringIO()
        with self.probe.Session([sys.executable, "-u", "-c", server],
                                self.probe.Recorder(output, ["sentinel-secret"]), timeout=2) as session:
            self.assertEqual(session.request("initialize", {}), {"ok": True})
        self.assertNotIn("sentinel-secret", output.getvalue())
        self.assertIn("test/event", output.getvalue())

    def test_silent_process_times_out_and_is_reaped(self):
        started = time.monotonic()
        with self.probe.Session([sys.executable, "-c", "import time; time.sleep(60)"],
                                self.probe.Recorder(io.StringIO(), []), timeout=0.2) as session:
            with self.assertRaises(TimeoutError):
                session.request("initialize", {})
            process = session.process
        self.assertIsNotNone(process.poll())
        self.assertLess(time.monotonic() - started, 5)

    def test_eof_fails_without_waiting_for_the_deadline(self):
        with self.probe.Session([sys.executable, "-c", "pass"],
                                self.probe.Recorder(io.StringIO(), []), timeout=10) as session:
            started = time.monotonic()
            with self.assertRaises(EOFError):
                session.request("initialize", {})
            self.assertLess(time.monotonic() - started, 3)

    def test_expired_deadline_is_enforced_even_with_queued_notifications(self):
        with self.probe.Session([sys.executable, "-c", "import time; time.sleep(60)"],
                                self.probe.Recorder(io.StringIO(), [])) as session:
            session.events.put({"method": "progress", "params": {}})
            with self.assertRaises(TimeoutError):
                session.receive(time.monotonic() - 1)

    def test_notification_before_response_remains_available_to_waiter(self):
        server = """import sys,json
for line in sys.stdin:
    request=json.loads(line)
    print(json.dumps({'method':'turn/completed','params':{'turn':{'status':'interrupted'}}}),flush=True)
    print(json.dumps({'id':request['id'],'result':{}}),flush=True)
"""
        with self.probe.Session([sys.executable, "-u", "-c", server],
                                self.probe.Recorder(io.StringIO(), []), timeout=2) as session:
            session.request("turn/interrupt", {})
            self.assertTrue(hasattr(session, "wait_notification"), "probe must retain notifications")
            message = session.wait_notification("turn/completed")
            self.assertEqual(message["params"]["turn"]["status"], "interrupted")


if __name__ == "__main__":
    unittest.main()
