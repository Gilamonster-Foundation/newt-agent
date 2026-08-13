#!/usr/bin/env python3
"""Exercise Newt's bounded reasoning continuation on a loopback server."""

from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from qualification_harness import (
    REASONING_REPLAY_MARKER,
    first_request_errors,
    second_request_errors,
)


REASONING_MARKER = REASONING_REPLAY_MARKER


class CaptureServer(ThreadingHTTPServer):
    model: str
    request_file: Path
    second_request_file: Path
    request_count: int
    request_lock: threading.Lock


class Handler(BaseHTTPRequestHandler):
    server: CaptureServer

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def send_json(self, status: int, value: object) -> None:
        encoded = json.dumps(value).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        if urlparse(self.path).path == "/v1/models":
            self.send_json(
                200,
                {"object": "list", "data": [{"id": self.server.model, "object": "model"}]},
            )
        else:
            self.send_json(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        if urlparse(self.path).path != "/v1/chat/completions":
            self.send_json(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 16 * 1024 * 1024:
                raise ValueError("invalid request length")
            request = json.loads(self.rfile.read(length))
            if not isinstance(request, dict):
                raise ValueError("request is not an object")
        except (ValueError, json.JSONDecodeError) as exc:
            self.send_json(400, {"error": str(exc)})
            return
        with self.server.request_lock:
            request_index = self.server.request_count
            if request_index >= 2:
                self.send_json(409, {"error": "preflight already captured two requests"})
                return
            self.server.request_count += 1
            request_file = (
                self.server.request_file
                if request_index == 0
                else self.server.second_request_file
            )
            request_file.parent.mkdir(parents=True, exist_ok=True)
            temporary = request_file.with_suffix(request_file.suffix + ".tmp")
            temporary.write_text(json.dumps(request, indent=2) + "\n", encoding="utf-8")
            temporary.replace(request_file)

        if request_index == 0:
            self.send_json(
                200,
                {
                    "id": "newt-qualification-preflight-reasoning",
                    "object": "chat.completion",
                    "created": 0,
                    "model": self.server.model,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "reasoning_content": REASONING_MARKER,
                            },
                            "finish_reason": "length",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            )
            return

        self.send_json(
            200,
            {
                "id": "newt-qualification-preflight-final",
                "object": "chat.completion",
                "created": 0,
                "model": self.server.model,
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Qualification preflight complete.",
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3},
            },
        )
        threading.Thread(target=self.server.shutdown, daemon=True).start()


def serve(args: argparse.Namespace) -> int:
    if args.timeout_seconds <= 0:
        print("preflight server timeout must be positive", file=sys.stderr)
        return 2
    server = CaptureServer(("127.0.0.1", 0), Handler)
    server.model = args.model
    server.request_file = args.request_file
    server.second_request_file = args.second_request_file
    server.request_count = 0
    server.request_lock = threading.Lock()
    args.port_file.parent.mkdir(parents=True, exist_ok=True)
    args.port_file.write_text(f"{server.server_port}\n", encoding="utf-8")
    lifetime = threading.Timer(args.timeout_seconds, server.shutdown)
    lifetime.daemon = True
    lifetime.start()
    try:
        server.serve_forever()
    finally:
        lifetime.cancel()
        server.server_close()
    if server.request_count != 2:
        print(
            f"preflight server captured {server.request_count} of 2 requests "
            "before its lifetime expired",
            file=sys.stderr,
        )
        return 1
    return 0


def assert_request(args: argparse.Namespace) -> int:
    try:
        request = json.loads(args.request_file.read_text(encoding="utf-8"))
        second_request = json.loads(
            args.second_request_file.read_text(encoding="utf-8")
        )
    except (OSError, json.JSONDecodeError) as exc:
        print(f"preflight assertion failed: {exc}", file=sys.stderr)
        return 1
    if not isinstance(request, dict):
        print("preflight assertion failed: request is not an object", file=sys.stderr)
        return 1
    if not isinstance(second_request, dict):
        print("preflight assertion failed: second request is not an object", file=sys.stderr)
        return 1
    errors = first_request_errors(request, args.model)
    errors.extend(second_request_errors(second_request, args.model, request))
    if errors:
        print("preflight assertion failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(
        "preflight assertion passed: Nemotron cognition policy and bounded "
        "reasoning continuation reached the wire"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    serve_parser = subparsers.add_parser("serve")
    serve_parser.add_argument("--port-file", required=True, type=Path)
    serve_parser.add_argument("--request-file", required=True, type=Path)
    serve_parser.add_argument("--second-request-file", required=True, type=Path)
    serve_parser.add_argument("--model", required=True)
    serve_parser.add_argument("--timeout-seconds", type=float, default=65.0)
    serve_parser.set_defaults(function=serve)
    assert_parser = subparsers.add_parser("assert-request")
    assert_parser.add_argument("--request-file", required=True, type=Path)
    assert_parser.add_argument("--second-request-file", required=True, type=Path)
    assert_parser.add_argument("--model", required=True)
    assert_parser.set_defaults(function=assert_request)
    args = parser.parse_args()
    return args.function(args)


if __name__ == "__main__":
    sys.exit(main())
