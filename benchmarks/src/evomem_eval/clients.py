from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import tempfile
import time
from contextlib import AbstractContextManager
from pathlib import Path
from typing import Any, Self

import httpx


def redact(value: Any) -> Any:
    secret_words = ("api_key", "apikey", "authorization", "token", "password")
    if isinstance(value, dict):
        return {
            k: "[REDACTED]" if any(x in k.lower() for x in secret_words) else redact(v)
            for k, v in value.items()
        }
    if isinstance(value, list):
        return [redact(v) for v in value]
    return value


class MCPClient:
    def __init__(self, url: str, namespace: str, timeout: float = 30):
        self.url, self.namespace = url, namespace
        headers = {"Accept": "application/json, text/event-stream"}
        if authorization := os.environ.get("EVOMEM_EVAL_MCP_AUTHORIZATION"):
            headers["Authorization"] = authorization
        self.client = httpx.Client(timeout=timeout, headers=headers)
        self.session = ""
        self.request_id = 0

    def close(self) -> None:
        self.client.close()

    def _post(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        author: str | None = None,
        notification: bool = False,
    ) -> Any:
        self.request_id += 1
        body: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if not notification:
            body["id"] = self.request_id
        if params is not None:
            body["params"] = params
        headers = {"X-Evomem-Namespace": self.namespace}
        if author:
            headers["X-Evomem-Author"] = author
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        response = self.client.post(self.url, json=body, headers=headers)
        response.raise_for_status()
        self.session = response.headers.get("Mcp-Session-Id", self.session)
        if not response.content:
            return None
        data = decode_mcp_response(response)
        if data.get("error"):
            raise RuntimeError(str(data["error"]))
        return data.get("result")

    def initialize(self) -> None:
        self._post(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "evomem-eval", "version": "0.1"},
            },
        )
        self._post("notifications/initialized", notification=True)

    def call(
        self, name: str, arguments: dict[str, Any], author: str | None = None
    ) -> Any:
        result = self._post(
            "tools/call", {"name": name, "arguments": arguments}, author
        )
        if result.get("isError"):
            text = " ".join(x.get("text", "") for x in result.get("content", []))
            raise RuntimeError(text or f"{name} failed")
        return result.get("structuredContent", result)


def decode_mcp_response(response: httpx.Response) -> dict[str, Any]:
    if "text/event-stream" not in response.headers.get("content-type", ""):
        return response.json()
    payloads = [
        line[5:].strip()
        for line in response.text.splitlines()
        if line.startswith("data:")
    ]
    if not payloads:
        raise ValueError("MCP SSE response contained no data event")
    return json.loads(payloads[-1])


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class EvomemProcess(AbstractContextManager["EvomemProcess"]):
    def __init__(self, binary: Path, root: Path):
        self.binary, self.root, self.port = binary, root, free_port()
        self.process: subprocess.Popen[str] | None = None

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/mcp"

    def __enter__(self) -> Self:
        env = os.environ | {
            "EVOMEM_ROOT": str(self.root),
            "BIND": f"127.0.0.1:{self.port}",
        }
        self.process = subprocess.Popen(
            [str(self.binary)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(
                    self.process.stderr.read()
                    if self.process.stderr
                    else "evomem exited"
                )
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                    return self
            except OSError:
                time.sleep(0.05)
        self.__exit__(None, None, None)
        raise TimeoutError("evomem server did not start within 20 seconds")

    def __exit__(self, *args: object) -> None:
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)


class ChatClient:
    def __init__(self, prefix: str):
        self.base_url = os.environ.get(f"EVOMEM_EVAL_{prefix}_BASE_URL", "").rstrip("/")
        self.model = os.environ.get(f"EVOMEM_EVAL_{prefix}_MODEL", "")
        self.api_key = os.environ.get(f"EVOMEM_EVAL_{prefix}_API_KEY", "")
        if not self.base_url or not self.model:
            raise ValueError(
                f"EVOMEM_EVAL_{prefix}_BASE_URL and EVOMEM_EVAL_{prefix}_MODEL are required"
            )

    def json(self, system: str, prompt: str) -> dict[str, Any]:
        headers = {"Authorization": f"Bearer {self.api_key}"} if self.api_key else {}
        body = {
            "model": self.model,
            "temperature": 0,
            "seed": 42,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt},
            ],
        }
        with httpx.Client(timeout=90) as client:
            response = client.post(
                f"{self.base_url}/chat/completions", headers=headers, json=body
            )
            response.raise_for_status()
            content = response.json()["choices"][0]["message"]["content"]
        match = re.search(r"\{.*\}", content, re.DOTALL)
        if not match:
            raise ValueError("model did not return a JSON object")
        return json.loads(match.group())


def temporary_root() -> tempfile.TemporaryDirectory[str]:
    return tempfile.TemporaryDirectory(prefix="evomem-eval-")
