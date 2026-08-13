#!/usr/bin/env python3
"""Offline tests for evidence capture that happens before/after a live run."""

from __future__ import annotations

import json
import hashlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
HARNESS = HERE.parent / "qualification_harness.py"
sys.path.insert(0, str(HERE.parent))

from qualification_harness import (  # noqa: E402
    HARNESS_FILES,
    PREFLIGHT_PROMPT,
    REASONING_REPLAY_MARKER,
    first_request_errors,
    generated_config_errors,
    second_request_errors,
    server_probe_errors,
)


class EvidenceCaptureTests(unittest.TestCase):
    def make_sources(self, root: Path) -> tuple[Path, Path]:
        harness = root / "harness"
        tasks = root / "tasks"
        harness.mkdir()
        tasks.mkdir()
        for name in HARNESS_FILES:
            (harness / name).write_text(f"fixture:{name}\n", encoding="utf-8")
        task = tasks / "one"
        task.mkdir()
        (task / "instruction.txt").write_text("do it\n", encoding="utf-8")
        (task / "verify.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        (task / "setup.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        return harness, tasks

    def snapshot(self, phase: str, output: Path, harness: Path, tasks: Path):
        return subprocess.run(
            [
                "python3",
                str(HARNESS),
                "snapshot-sources",
                "--phase",
                phase,
                "--output",
                str(output),
                "--harness-root",
                str(harness),
                "--tasks-root",
                str(tasks),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

    def test_source_snapshot_accepts_unchanged_files_and_rejects_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            harness, tasks = self.make_sources(root)
            snapshot = root / "snapshot.json"
            before = self.snapshot("before", snapshot, harness, tasks)
            self.assertEqual(before.returncode, 0, before.stdout)
            unchanged = self.snapshot("after", snapshot, harness, tasks)
            self.assertEqual(unchanged.returncode, 0, unchanged.stdout)
            data = json.loads(snapshot.read_text(encoding="utf-8"))
            self.assertEqual(data["aggregate_sha256_before"], data["aggregate_sha256_after"])
            labels = {entry["path"] for entry in data["files"]}
            self.assertIn("tasks/one/instruction.txt", labels)
            self.assertIn("tasks/one/verify.sh", labels)
            self.assertIn("tasks/one/setup.sh", labels)

            # Recreate the baseline and mutate an executable source between phases.
            snapshot.unlink()
            self.assertEqual(self.snapshot("before", snapshot, harness, tasks).returncode, 0)
            (harness / "run-matrix.sh").write_text("changed\n", encoding="utf-8")
            changed = self.snapshot("after", snapshot, harness, tasks)
            self.assertNotEqual(changed.returncode, 0, changed.stdout)
            data = json.loads(snapshot.read_text(encoding="utf-8"))
            self.assertIn("changed:harness/run-matrix.sh", data["differences"])

    def test_generated_config_accepts_only_the_runner_shape(self) -> None:
        endpoint = "http://127.0.0.1:8000"
        model = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16"
        lines = [
            'default_backend = "nemotron"',
            "[[backends]]",
            'name = "nemotron"',
            f'endpoint = "{endpoint}"',
            f'model = "{model}"',
            'kind = "openai"',
            'api = "chat_completions"',
            'tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]',
            "[backends.capability]",
            'reasoning_replay_scope = "current_user_turn"',
            "[backends.capability.chat_completions]",
            "cognition = true",
            "chat_template_kwargs = true",
            "parallel_tool_calls = false",
            "bounded_reasoning_continuation = true",
        ]
        text = "\n".join(lines) + "\n"
        self.assertEqual(
            generated_config_errors(text, "nemotron", endpoint, model), []
        )

        with_key = list(lines)
        with_key.insert(7, 'api_key_file = "/run/secrets/model-key"')
        self.assertEqual(
            generated_config_errors(
                "\n".join(with_key) + "\n", "nemotron", endpoint, model
            ),
            [],
        )
        self.assertTrue(
            generated_config_errors(
                "\n".join(with_key) + "\n",
                "nemotron",
                endpoint,
                model,
                allow_api_key_file=False,
            )
        )

        invalid = {
            "comments": "\n".join(f"# {line}" for line in lines) + "\n",
            "extra": text + "unexpected = true\n",
            "wrong endpoint": text.replace(endpoint, "http://foreign.invalid"),
            "malformed api key": text.replace(
                lines[7], 'api_key_file = "unterminated\n' + lines[7]
            ),
        }
        for name, candidate in invalid.items():
            with self.subTest(name=name):
                self.assertTrue(
                    generated_config_errors(candidate, "nemotron", endpoint, model)
                )

    def test_preflight_requests_bind_prompt_tools_and_current_turn_replay(self) -> None:
        model = "nemotron-fixture"
        tool = {
            "type": "function",
            "function": {
                "name": "list_dir",
                "parameters": {"type": "object", "properties": {}},
            },
        }
        first = {
            "model": model,
            "messages": [
                {"role": "system", "content": "fixture system"},
                {"role": "user", "content": PREFLIGHT_PROMPT},
            ],
            "tools": [tool],
            "tool_choice": "auto",
            "max_tokens": 16000,
            "temperature": 0.6,
            "top_p": 0.95,
            "parallel_tool_calls": False,
            "chat_template_kwargs": {
                "enable_thinking": True,
                "truncate_history_thinking": True,
            },
        }
        second = dict(first)
        second["messages"] = first["messages"] + [
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": REASONING_REPLAY_MARKER,
            }
        ]
        self.assertEqual(first_request_errors(first, model), [])
        self.assertEqual(second_request_errors(second, model, first), [])

        # Newt's prompt-provenance shape (prompt_read.rs): the exact operator
        # text appears once as the compression-proof recovery copy right after
        # the [NEWT ACTIVE PROMPT …] system card AND once as the live tail
        # turn. That intended duplication is valid evidence.
        card_pair = {
            **first,
            "messages": [
                {
                    "role": "system",
                    "content": "[NEWT ACTIVE PROMPT v1]\naddress: prompt:fixture",
                },
                {"role": "user", "content": PREFLIGHT_PROMPT},
                {"role": "user", "content": PREFLIGHT_PROMPT},
            ],
        }
        self.assertEqual(
            first_request_errors(card_pair, model),
            [],
            "card-adjacent recovery copy + live tail turn is Newt's intended shape",
        )

        mutations = {
            "wrong prompt": {
                **first,
                "messages": [{"role": "user", "content": "some other task"}],
            },
            "duplicate without its provenance card": {
                **first,
                "messages": [
                    {"role": "system", "content": "fixture system"},
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                ],
            },
            "three copies of the prompt": {
                **first,
                "messages": [
                    {
                        "role": "system",
                        "content": "[NEWT ACTIVE PROMPT v1]\naddress: prompt:fixture",
                    },
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                ],
            },
            "prompt not the live tail turn": {
                **first,
                "messages": [
                    {"role": "user", "content": PREFLIGHT_PROMPT},
                    {"role": "user", "content": "a later unrelated turn"},
                ],
            },
            "marker in first": {
                **first,
                "messages": first["messages"]
                + [
                    {
                        "role": "assistant",
                        "reasoning_content": REASONING_REPLAY_MARKER,
                    }
                ],
            },
            "malformed tool": {**first, "tools": [{"type": "function"}]},
            "invalid tool name": {
                **first,
                "tools": [
                    {
                        "type": "function",
                        "function": {"name": "bad tool name", "parameters": {}},
                    }
                ],
            },
        }
        for name, request in mutations.items():
            with self.subTest(name=name):
                self.assertTrue(first_request_errors(request, model))

        second_mutations = {
            "changed prior message": {
                **second,
                "messages": [
                    {"role": "system", "content": "changed"},
                    *second["messages"][1:],
                ],
            },
            "extra message": {
                **second,
                "messages": second["messages"]
                + [{"role": "assistant", "content": "extra"}],
            },
            "replay not appended": {
                **second,
                "messages": [second["messages"][-1], *first["messages"]],
            },
        }
        for name, request in second_mutations.items():
            with self.subTest(name=name):
                self.assertTrue(second_request_errors(request, model, first))

    def test_llama_props_ground_version_context_and_exact_template_bytes(self) -> None:
        template = "{% for message in messages %}{{ message.content }}{% endfor %}\n"
        template_id = "sha256:" + hashlib.sha256(template.encode("utf-8")).hexdigest()
        props = {
            "build_info": "b1",
            "default_generation_settings": {"n_ctx": 65536},
            "chat_template": template,
        }
        self.assertEqual(
            server_probe_errors(
                "llama_cpp",
                "b1",
                65536,
                props_response=props,
                chat_template_id=template_id,
            ),
            [],
        )

        required = {
            "missing build_info": ({**props, "build_info": None}, "build_info"),
            "missing n_ctx": (
                {**props, "default_generation_settings": {}},
                "n_ctx",
            ),
            "wrong context": (
                {**props, "default_generation_settings": {"n_ctx": 32768}},
                "n_ctx",
            ),
            "wrong template": ({**props, "chat_template": template + "changed"}, "chat template"),
        }
        for name, (candidate, expected) in required.items():
            with self.subTest(name=name):
                errors = server_probe_errors(
                    "llama_cpp",
                    "b1",
                    65536,
                    props_response=candidate,
                    chat_template_id=template_id,
                )
                self.assertTrue(any(expected in error for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
