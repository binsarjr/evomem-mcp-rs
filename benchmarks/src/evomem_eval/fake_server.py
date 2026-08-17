from __future__ import annotations

import json
import threading
from contextlib import AbstractContextManager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Self

from .clients import free_port


class _Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        prompt = body["messages"][-1]["content"]
        marker = "FAKE_EXPECTED_JSON:"
        expected = (
            json.loads(prompt.split(marker, 1)[1].splitlines()[0])
            if marker in prompt
            else {}
        )
        if "pairwise" in body["messages"][0]["content"].lower():
            payload = {"winner": "tie", "reason": "equivalent"}
        elif "memory writer" in body["messages"][0]["content"].lower():
            event_marker = "FAKE_EVENT_JSON:"
            event = json.loads(prompt.split(event_marker, 1)[1].splitlines()[0])
            payload = {"memories": [event["text"]]}
        elif "judge" in body["messages"][0]["content"].lower():
            payload: dict[str, Any] = {
                key: 100
                for key in (
                    "correctness",
                    "faithfulness",
                    "completeness",
                    "abstention",
                    "provenance",
                    "conflict_disclosure",
                    "duplicate_free",
                )
            }
        else:
            payload = {
                "answer": expected.get("answer", "unknown"),
                "citations": expected.get("evidence_ids", []),
            }
        encoded = json.dumps(
            {"choices": [{"message": {"content": json.dumps(payload)}}]}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, *_: Any) -> None:
        pass


class FakeOpenAI(AbstractContextManager["FakeOpenAI"]):
    def __init__(self) -> None:
        self.port = free_port()
        self.server = ThreadingHTTPServer(("127.0.0.1", self.port), _Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> Self:
        self.thread.start()
        return self

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1"

    def __exit__(self, *args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
