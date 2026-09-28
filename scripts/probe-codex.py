#!/usr/bin/env python3
"""Bounded, sanitized app-server capture. Live output still needs human review.

No prompt is sent unless --prompt is supplied. Run in a disposable working
directory; approvals are declined unless --approve is explicitly supplied.
Credentials are read only from the child environment/CODEX_HOME, never argv.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time


class Recorder:
    def __init__(self, output, secrets):
        self.output = output
        self.secrets = sorted((s for s in secrets if s), key=len, reverse=True)
        self.lock = threading.Lock()

    def clean(self, value):
        if isinstance(value, dict):
            return {key: "<redacted>" if any(word in key.lower() for word in
                    ("token", "api_key", "apikey", "authorization", "password"))
                    and not isinstance(item, (dict, list, int, float))
                    else self.clean(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.clean(item) for item in value]
        if isinstance(value, str):
            for secret in self.secrets:
                value = value.replace(secret, "<redacted>")
        return value

    def write(self, direction, message):
        with self.lock:
            self.output.write(json.dumps({"direction": direction,
                                         "message": self.clean(message)}) + "\n")
            self.output.flush()


class Session:
    def __init__(self, argv, recorder, timeout=15, approve=False):
        self.recorder = recorder
        self.timeout = timeout
        self.approve = approve
        self.sequence = 0
        self.notifications = []
        self.events = queue.Queue()
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        text=True, encoding="utf-8", errors="replace", bufsize=1)
        self.readers = [threading.Thread(target=self._read, args=(stream, direction), daemon=True)
                        for stream, direction in [(self.process.stdout, "recv"),
                                                  (self.process.stderr, "stderr")]]
        for reader in self.readers:
            reader.start()

    def _read(self, stream, direction):
        try:
            for line in stream:
                try:
                    message = json.loads(line) if direction == "recv" else line.rstrip()
                except ValueError:
                    message = {"invalid_json": line.rstrip()}
                self.recorder.write(direction, message)
                if direction == "recv":
                    self.events.put(message)
        finally:
            if direction == "recv":
                self.events.put(None)

    def send(self, message):
        self.recorder.write("send", message)
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def receive(self, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("app-server response deadline exceeded")
        try:
            message = self.events.get(timeout=remaining)
        except queue.Empty:
            raise TimeoutError("app-server response deadline exceeded") from None
        if message is None:
            raise EOFError("app-server closed stdout")
        if not isinstance(message, dict):
            raise ValueError("app-server sent a non-object message")
        if "method" in message and "id" in message:
            if message["method"] in ("item/commandExecution/requestApproval",
                                      "item/fileChange/requestApproval"):
                self.send({"id": message["id"], "result": {
                    "decision": "accept" if self.approve else "decline"}})
            else:
                self.send({"id": message["id"], "error": {
                    "code": -32601, "message": "unsupported by preflight probe"}})
        return message

    def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.send({"id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + self.timeout
        while True:
            message = self.receive(deadline)
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(self.recorder.clean(json.dumps(message["error"])))
                return message["result"]
            if "method" in message and "id" not in message:
                self.notifications.append(message)

    def wait_notification(self, method):
        for index, message in enumerate(self.notifications):
            if message.get("method") == method:
                return self.notifications.pop(index)
        deadline = time.monotonic() + self.timeout
        while True:
            message = self.receive(deadline)
            if message.get("method") == method:
                return message

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=2)
        for reader in self.readers:
            reader.join(timeout=1)
        for stream in (self.process.stdout, self.process.stderr):
            stream.close()


def credential_secrets():
    secrets = [os.environ.get("OPENAI_API_KEY", "")]
    home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    try:
        auth = json.loads((home / "auth.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return secrets
    def collect(value):
        if isinstance(value, dict):
            for key, item in value.items():
                if isinstance(item, str) and any(word in key.lower() for word in
                                                ("token", "key", "account", "email")):
                    secrets.append(item)
                elif isinstance(item, dict):
                    collect(item)
    collect(auth)
    return secrets


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--output", required=True)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--prompt")
    parser.add_argument("--resume")
    parser.add_argument("--model")
    parser.add_argument("--approve", action="store_true")
    parser.add_argument("--exercise-control", action="store_true")
    parser.add_argument("--sandbox", choices=["read-only", "danger-full-access"], default="danger-full-access")
    parser.add_argument("--approval-policy", choices=["never", "on-request"], default="on-request")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    with open(args.output, "w", encoding="utf-8") as output:
        record = Recorder(output, credential_secrets())
        with Session([args.codex, "app-server"], record, args.timeout, args.approve) as session:
            session.request("initialize", {"clientInfo": {
                "name": "bondsymphonic_preflight", "version": "0.1.0"}})
            session.send({"method": "initialized", "params": {}})
            session.request("model/list", {"limit": 100})
            if args.prompt:
                params = {"cwd": str(Path.cwd()), "sandbox": args.sandbox,
                          "approvalPolicy": args.approval_policy}
                if args.model:
                    params["model"] = args.model
                if args.resume:
                    params["threadId"] = args.resume
                thread = session.request("thread/resume" if args.resume else "thread/start", params)
                session.request("turn/start", {"threadId": thread["thread"]["id"], "input": [
                    {"type": "text", "text": args.prompt}]})
                completed = session.wait_notification("turn/completed")
                if completed["params"]["turn"]["status"] != "completed":
                    raise RuntimeError("preflight turn did not complete successfully")
                if args.exercise_control:
                    thread_id = thread["thread"]["id"]
                    session.request("thread/resume", {"threadId": thread_id,
                                                      "sandbox": args.sandbox,
                                                      "approvalPolicy": args.approval_policy})
                    turn = session.request("turn/start", {"threadId": thread_id, "input": [
                        {"type": "text", "text": "Run sleep 30 in the shell, then reply CONTROL_DONE. Change no files."}]})
                    turn_id = turn["turn"]["id"]
                    # turn/start acknowledges before the worker has activated
                    # the turn. Interrupting before turn/started races startup.
                    while session.wait_notification("turn/started")["params"]["turn"]["id"] != turn_id:
                        pass
                    session.request("turn/steer", {"threadId": thread_id,
                        "expectedTurnId": turn_id, "input": [
                            {"type": "text", "text": "Reply STEERED_DONE instead when finished."}]})
                    session.request("turn/interrupt", {"threadId": thread_id, "turnId": turn_id})
                    interrupted = session.wait_notification("turn/completed")
                    if interrupted["params"]["turn"]["status"] != "interrupted":
                        raise RuntimeError("preflight interrupt was not observed")


if __name__ == "__main__":
    main()
