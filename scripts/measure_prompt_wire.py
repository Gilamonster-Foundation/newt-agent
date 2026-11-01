#!/usr/bin/env python3
"""Recording loopback provider and request analyser for measure-prompt-presets.sh.

  measure_prompt_wire.py serve DIR     answer /v1/models and /v1/chat/completions on a
                                       free loopback port; write the port to DIR/port and
                                       append every chat request body to DIR/requests.jsonl
  measure_prompt_wire.py report DIR M  print the sizes of the first chat request whose
                                       messages contain the marker M:
                                       system_chars tools tools_chars body_chars

Sizes are characters of what newt actually sent: every system message, the `tools`
array as compact JSON, and the whole request body.
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MODEL = "measure-fixture"
LOCK = threading.Lock()


def serve(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    log = directory / "requests.jsonl"

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):  # quiet
            pass

        def _send(self, code: int, body: bytes, ctype: str = "application/json") -> None:
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:
            if self.path.rstrip("/").endswith("/models"):
                self._send(200, json.dumps({"object": "list", "data": [{"id": MODEL, "object": "model"}]}).encode())
            else:
                self._send(404, b"{}")

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if not self.path.endswith("/chat/completions"):
                self._send(404, b"{}")
                return
            text = raw.decode("utf-8", "replace")
            with LOCK, log.open("a") as f:
                f.write(json.dumps({"body": text}) + "\n")
            try:
                streaming = bool(json.loads(text).get("stream"))
            except ValueError:
                streaming = False
            answer = "Measured."
            if streaming:
                frame = {"choices": [{"delta": {"content": answer}, "finish_reason": "stop"}]}
                usage = {"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}
                body = f"data: {json.dumps(frame)}\n\ndata: {json.dumps(usage)}\n\ndata: [DONE]\n\n".encode()
                self._send(200, body, "text/event-stream")
            else:
                reply = {
                    "model": MODEL,
                    "choices": [{"message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                }
                self._send(200, json.dumps(reply).encode())

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    (directory / "port").write_text(str(server.server_address[1]))
    server.serve_forever()


def content_chars(message: dict) -> int:
    content = message.get("content")
    if isinstance(content, str):
        return len(content)
    if isinstance(content, list):
        return sum(len(p.get("text", "")) for p in content if isinstance(p, dict))
    return 0


def report(directory: Path, marker: str) -> int:
    log = directory / "requests.jsonl"
    if not log.exists():
        return 1
    for line in log.read_text().splitlines():
        body = json.loads(line)["body"]
        request = json.loads(body)
        messages = request.get("messages", [])
        if not any(marker in json.dumps(m) for m in messages if m.get("role") == "user"):
            continue
        system = sum(content_chars(m) for m in messages if m.get("role") == "system")
        tools = request.get("tools") or []
        tools_chars = len(json.dumps(tools, separators=(",", ":"), ensure_ascii=False))
        print(system, len(tools), tools_chars, len(body))
        return 0
    return 1


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "serve":
        serve(Path(sys.argv[2]))
    elif len(sys.argv) == 4 and sys.argv[1] == "report":
        sys.exit(report(Path(sys.argv[2]), sys.argv[3]))
    else:
        sys.exit(__doc__)
