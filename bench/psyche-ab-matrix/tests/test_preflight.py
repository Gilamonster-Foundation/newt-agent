#!/usr/bin/env python3
"""Real-loopback tests for the bounded-continuation preflight server."""

from __future__ import annotations

import json
import subprocess
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path


HERE = Path(__file__).resolve().parent
SERVER = HERE.parent / "loopback-preflight.py"
REASONING_MARKER = "NEWT_QUALIFICATION_REASONING_REPLAY_MARKER_v1"
PREFLIGHT_PROMPT = (
    "Answer briefly that the qualification preflight is complete. Do not call a tool."
)
FUNCTION_TOOL = {
    "type": "function",
    "function": {
        "name": "list_dir",
        "parameters": {"type": "object", "properties": {}},
    },
}


class LoopbackPreflightTests(unittest.TestCase):
    def test_server_captures_reasoning_continuation_as_two_requests(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            port_file = root / "port"
            first_request_file = root / "first-request.json"
            second_request_file = root / "second-request.json"
            model = "nemotron-fixture"
            process = subprocess.Popen(
                [
                    "python3",
                    str(SERVER),
                    "serve",
                    "--port-file",
                    str(port_file),
                    "--request-file",
                    str(first_request_file),
                    "--second-request-file",
                    str(second_request_file),
                    "--model",
                    model,
                    "--timeout-seconds",
                    "2",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                for _ in range(100):
                    if port_file.exists():
                        break
                    if process.poll() is not None:
                        self.fail("loopback server exited before publishing its port")
                    time.sleep(0.01)
                port = int(port_file.read_text(encoding="utf-8"))
                body = {
                    "model": model,
                    "messages": [{"role": "user", "content": PREFLIGHT_PROMPT}],
                    "tools": [FUNCTION_TOOL],
                }
                req = urllib.request.Request(
                    f"http://127.0.0.1:{port}/v1/chat/completions",
                    data=json.dumps(body).encode(),
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(req, timeout=2) as response:
                    reply = json.load(response)
                self.assertEqual(reply["model"], model)
                self.assertEqual(reply["choices"][0]["finish_reason"], "length")
                self.assertIsNone(reply["choices"][0]["message"]["content"])
                self.assertEqual(
                    reply["choices"][0]["message"]["reasoning_content"],
                    REASONING_MARKER,
                )
                self.assertEqual(
                    json.loads(first_request_file.read_text(encoding="utf-8")), body
                )
                self.assertIsNone(process.poll(), "server must await the continuation")

                continuation = {
                    "model": model,
                    "messages": [
                        {"role": "user", "content": PREFLIGHT_PROMPT},
                        {
                            "role": "assistant",
                            "content": None,
                            "reasoning_content": REASONING_MARKER,
                        }
                    ],
                    "tools": [FUNCTION_TOOL],
                }
                continuation_req = urllib.request.Request(
                    f"http://127.0.0.1:{port}/v1/chat/completions",
                    data=json.dumps(continuation).encode(),
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(continuation_req, timeout=2) as response:
                    final_reply = json.load(response)
                self.assertEqual(final_reply["choices"][0]["finish_reason"], "stop")
                self.assertEqual(
                    final_reply["choices"][0]["message"]["content"],
                    "Qualification preflight complete.",
                )
                self.assertEqual(
                    json.loads(second_request_file.read_text(encoding="utf-8")),
                    continuation,
                )
                self.assertEqual(process.wait(timeout=2), 0)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=2)

    def test_server_lifetime_is_bounded_with_zero_or_one_post(self) -> None:
        for post_count in (0, 1):
            with self.subTest(post_count=post_count), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                port_file = root / "port"
                first_request_file = root / "first-request.json"
                second_request_file = root / "second-request.json"
                started = time.monotonic()
                process = subprocess.Popen(
                    [
                        "python3",
                        str(SERVER),
                        "serve",
                        "--port-file",
                        str(port_file),
                        "--request-file",
                        str(first_request_file),
                        "--second-request-file",
                        str(second_request_file),
                        "--model",
                        "nemotron-fixture",
                        "--timeout-seconds",
                        "0.2",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                )
                try:
                    for _ in range(100):
                        if port_file.exists():
                            break
                        if process.poll() is not None:
                            break
                        time.sleep(0.005)
                    if not port_file.exists():
                        output = process.communicate(timeout=2)[0]
                        self.fail(f"preflight server did not publish its port: {output}")
                    if post_count:
                        port = int(port_file.read_text(encoding="utf-8"))
                        request = urllib.request.Request(
                            f"http://127.0.0.1:{port}/v1/chat/completions",
                            data=json.dumps({"messages": []}).encode(),
                            headers={"Content-Type": "application/json"},
                        )
                        with urllib.request.urlopen(request, timeout=1):
                            pass
                    output = process.communicate(timeout=2)[0]
                    self.assertNotEqual(process.returncode, 0, output)
                    self.assertIn(f"captured {post_count} of 2 requests", output)
                    self.assertLess(time.monotonic() - started, 1.5)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        process.wait(timeout=2)

    def test_assert_request_requires_marker_in_second_request(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first_request_file = root / "first-request.json"
            second_request_file = root / "second-request.json"
            model = "nemotron-fixture"
            first_request_file.write_text(
                json.dumps(
                    {
                        "model": model,
                        "messages": [{"role": "user", "content": PREFLIGHT_PROMPT}],
                        "max_tokens": 16000,
                        "parallel_tool_calls": False,
                        "temperature": 0.6,
                        "top_p": 0.95,
                        "chat_template_kwargs": {
                            "enable_thinking": True,
                            "truncate_history_thinking": True,
                        },
                        "tools": [FUNCTION_TOOL],
                        "tool_choice": "auto",
                    }
                ),
                encoding="utf-8",
            )

            command = [
                "python3",
                str(SERVER),
                "assert-request",
                "--request-file",
                str(first_request_file),
                "--second-request-file",
                str(second_request_file),
                "--model",
                model,
            ]
            second_request_file.write_text(
                json.dumps(
                    {
                        "model": model,
                        "max_tokens": 16000,
                        "parallel_tool_calls": False,
                        "temperature": 0.6,
                        "top_p": 0.95,
                        "chat_template_kwargs": {
                            "enable_thinking": True,
                            "truncate_history_thinking": True,
                        },
                        "tools": [FUNCTION_TOOL],
                        "tool_choice": "auto",
                        "messages": [
                            {"role": "user", "content": PREFLIGHT_PROMPT},
                            {
                                "role": "assistant",
                                "content": None,
                                "reasoning_content": REASONING_MARKER,
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            passed = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(passed.returncode, 0, passed.stderr)

            second_request_file.write_text(
                json.dumps({"messages": [{"role": "assistant", "content": None}]}),
                encoding="utf-8",
            )
            failed = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(failed.returncode, 1)
            self.assertIn("reasoning replay marker", failed.stderr)


if __name__ == "__main__":
    unittest.main()
