#!/usr/bin/env python3
"""Offline positive and adversarial fixtures for the qualification gate."""

from __future__ import annotations

import csv
import hashlib
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
VALIDATOR = HERE.parent / "validate-run.py"
MODEL = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16"
PREFLIGHT_PROMPT = (
    "Answer briefly that the qualification preflight is complete. Do not call a tool."
)
DIGEST = "b" * 64
BINARY_DIGEST = "a" * 64
SOURCE_DIGEST = "e" * 64
POSTURES = ["baseline", "tenacity", "crew", "obsessive"]
OCAPS = ["off", "on"]
TENACITY = {
    "baseline": "standard",
    "tenacity": "relentless",
    "crew": "standard",
    "obsessive": "relentless",
}
COGNITION = {
    "baseline": "default",
    "tenacity": "default",
    "crew": "default",
    "obsessive": "contemplating",
}
CREW = {
    "baseline": "off",
    "tenacity": "off",
    "crew": "on",
    "obsessive": "on",
}
HARNESS_SOURCE_FILES = (
    ".gitignore",
    "README.md",
    "loopback-preflight.py",
    "qualification_harness.py",
    "run-matrix.sh",
    "validate-run.py",
)


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_aggregate(entries: list[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for label, file_digest in sorted(entries):
        digest.update(label.encode())
        digest.update(b"\0")
        digest.update(file_digest.encode())
        digest.update(b"\n")
    return digest.hexdigest()


def cell_name(posture: str, ocap: str) -> str:
    return f"{posture}-{ocap}-write-greeting"


def config_text(endpoint: str) -> str:
    return f'''default_backend = "nemotron"
[[backends]]
name = "nemotron"
endpoint = "{endpoint}"
model = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16"
kind = "openai"
api = "chat_completions"
tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
[backends.capability]
reasoning_replay_scope = "current_user_turn"
[backends.capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
'''


def make_valid_fixture(root: Path) -> None:
    launch = {
        "schema_version": 1,
        "model_id": MODEL,
        "model_digest": DIGEST,
        "context_window": 65536,
        "server_kind": "vllm",
        "server_version": "0.19.0",
        "chat_template_id": "nemotron-v3@sha256:fixture",
        "tool_parser_id": "qwen3_coder",
        "reasoning_parser_id": "nano_v3",
    }
    write_json(root / "server-launch-manifest.json", launch)
    launch_digest = sha256(root / "server-launch-manifest.json")
    write_json(
        root / "manifest.json",
        {
            "schema_version": 1,
            "mode": "qualification",
            "model": {
                "requested_id": MODEL,
                "digest": DIGEST,
                "context_window": 65536,
            },
            "backend": {
                "name": "nemotron",
                "api": "chat_completions",
                "capability_profile": "nemotron",
            },
            "server": {
                "endpoint": "http://127.0.0.1:8000",
                "kind": "vllm",
                "version": "0.19.0",
                "chat_template_id": "nemotron-v3@sha256:fixture",
                "tool_parser_id": "qwen3_coder",
                "reasoning_parser_id": "nano_v3",
                "launch_manifest_sha256": launch_digest,
            },
            "matrix": {
                "postures": POSTURES,
                "ocap_modes": OCAPS,
                "tasks": ["write-greeting"],
                "tasks_dir": "/fixture/canonical/tasks",
                "max_rounds": 15,
            },
        },
    )
    source_entries = [
        *((f"harness/{name}", SOURCE_DIGEST) for name in HARNESS_SOURCE_FILES),
        ("tasks/write-greeting/instruction.txt", SOURCE_DIGEST),
        ("tasks/write-greeting/verify.sh", SOURCE_DIGEST),
    ]
    aggregate = source_aggregate(source_entries)
    write_json(
        root / "harness-sources.json",
        {
            "schema_version": 1,
            "files": [
                {
                    "path": label,
                    "sha256_before": digest,
                    "sha256_after": digest,
                }
                for label, digest in source_entries
            ],
            "aggregate_sha256_before": aggregate,
            "aggregate_sha256_after": aggregate,
            "differences": [],
        },
    )
    write_json(
        root / "provenance.json",
        {
            "schema_version": 1,
            "captured_at_utc": "2026-08-01T00:00:00Z",
            "model": {
                "id": MODEL,
                "digest": DIGEST,
                "context_window": 65536,
            },
            "server": {
                "endpoint": "http://127.0.0.1:8000",
                "kind": "vllm",
                "version": "0.19.0",
                "chat_template_id": "nemotron-v3@sha256:fixture",
                "tool_parser_id": "qwen3_coder",
                "reasoning_parser_id": "nano_v3",
                "probe_ok": True,
                "launch_manifest": {
                    "artifact": "server-launch-manifest.json",
                    "sha256": launch_digest,
                },
            },
            "newt": {
                "path": "/opt/newt/bin/newt",
                "version": "newt 0.7.6 (fixture)",
                "sha256_before": BINARY_DIGEST,
                "sha256_after": BINARY_DIGEST,
            },
            "harness": {
                "commit": "c" * 40,
                "dirty": False,
                "source_manifest": "harness-sources.json",
                "source_sha256_before": aggregate,
                "source_sha256_after": aggregate,
            },
        },
    )
    (root / "config.toml").write_text(
        config_text("http://127.0.0.1:8000"), encoding="utf-8"
    )
    write_json(
        root / "server-models.json",
        {"object": "list", "data": [{"id": MODEL, "object": "model"}]},
    )
    write_json(root / "server-version.json", {"version": "0.19.0"})
    (root / "preflight").mkdir()
    (root / "preflight" / "config.toml").write_text(
        config_text("http://127.0.0.1:43123"), encoding="utf-8"
    )
    (root / "preflight" / "port").write_text("43123\n", encoding="utf-8")
    (root / "preflight" / "instruction.txt").write_text(
        "Complete the preflight.\n", encoding="utf-8"
    )
    (root / "preflight" / "server.trace").write_text(
        "fixture server trace\n", encoding="utf-8"
    )
    (root / "preflight" / "newt.trace").write_text(
        "fixture newt trace\n", encoding="utf-8"
    )
    (root / "preflight" / "ws").mkdir()
    write_json(
        root / "preflight" / "first-request.json",
        {
            "model": MODEL,
            "messages": [{"role": "user", "content": PREFLIGHT_PROMPT}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "list_dir",
                        "parameters": {"type": "object", "properties": {}},
                    },
                }
            ],
            "tool_choice": "auto",
            "max_tokens": 16000,
            "temperature": 0.6,
            "top_p": 0.95,
            "parallel_tool_calls": False,
            "chat_template_kwargs": {
                "enable_thinking": True,
                "truncate_history_thinking": True,
            },
        },
    )
    write_json(
        root / "preflight" / "second-request.json",
        {
            "model": MODEL,
            "messages": [
                {"role": "user", "content": PREFLIGHT_PROMPT},
                {
                    "role": "assistant",
                    "content": None,
                    "reasoning_content": "NEWT_QUALIFICATION_REASONING_REPLAY_MARKER_v1",
                },
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "list_dir",
                        "parameters": {"type": "object", "properties": {}},
                    },
                }
            ],
            "tool_choice": "auto",
            "max_tokens": 16000,
            "temperature": 0.6,
            "top_p": 0.95,
            "parallel_tool_calls": False,
            "chat_template_kwargs": {
                "enable_thinking": True,
                "truncate_history_thinking": True,
            },
        },
    )
    preflight_records = [
        {
            "kind": "solve_result",
            "task_file": str((root / "preflight" / "instruction.txt").resolve()),
            "cwd": str((root / "preflight" / "ws").resolve()),
            "model": MODEL,
            "backend_kind": "openai",
            "status": "completed",
        },
        {
            "contract_version": "1",
            "requested_model": MODEL,
            "effective_model": MODEL,
            "model_digest": DIGEST,
            "outcome": "completed",
            "backend": {"name": "nemotron", "kind": "openai"},
            "agent": "newt-agent",
            "agent_version": "0.7.6",
            "effective_config": {
                "context_window": 65536,
                "tenacity": "standard",
                "cognition": "contemplating",
                "crew": "off",
                "ocap": "off",
                "max_rounds": 2,
            },
            "timing": {"wall_ms": 10},
        },
        {
            "kind": "reasoning_overflow",
            "round": 0,
            "reasoning_overflow_detected": True,
            "continuation_attempted": True,
            "continuation_succeeded": True,
            "finish_reason": "length",
            "reasoning_tokens_estimate": 1,
        },
    ]
    (root / "preflight" / "events.jsonl").write_text(
        "".join(json.dumps(record) + "\n" for record in preflight_records),
        encoding="utf-8",
    )

    (root / "events").mkdir()
    (root / "traces").mkdir()
    (root / "workspace-baselines").mkdir()
    (root / "matrix.md").write_text("# fixture matrix\n", encoding="utf-8")
    rows: list[dict[str, object]] = []
    for posture in POSTURES:
        for ocap in OCAPS:
            cell = cell_name(posture, ocap)
            records = [
                {
                    "kind": "solve_result",
                    "task_file": "/fixture/canonical/tasks/write-greeting/instruction.txt",
                    "cwd": str((root / "ws" / cell).resolve()),
                    "model": MODEL,
                    "backend_kind": "openai",
                    "status": "completed",
                },
                {
                    "contract_version": "1",
                    "requested_model": MODEL,
                    "effective_model": MODEL,
                    "model_digest": DIGEST,
                    "outcome": "completed",
                    "backend": {"name": "nemotron", "kind": "openai"},
                    "agent": "newt-agent",
                    "agent_version": "0.7.6",
                    "effective_config": {
                        "context_window": 65536,
                        "tenacity": TENACITY[posture],
                        "cognition": COGNITION[posture],
                        "crew": CREW[posture],
                        "ocap": ocap,
                        "max_rounds": 15,
                    },
                    "timing": {"wall_ms": 10},
                },
            ]
            (root / "events" / f"{cell}.jsonl").write_text(
                "".join(json.dumps(record) + "\n" for record in records),
                encoding="utf-8",
            )
            (root / "traces" / f"{cell}.trace").write_text(
                "fixture solve trace\n", encoding="utf-8"
            )
            (root / "traces" / f"{cell}.setup.trace").write_text(
                "fixture setup trace\n", encoding="utf-8"
            )
            workspace = str((root / "ws" / cell).resolve())
            write_json(
                root / "workspace-baselines" / f"{cell}.json",
                {
                    "schema_version": 1,
                    "cell": cell,
                    "posture": posture,
                    "ocap": ocap,
                    "task": "write-greeting",
                    "workspace": workspace,
                    "git": {
                        "root": workspace,
                        "baseline_commit": "d" * 40,
                        "baseline_tree": "e" * 40,
                        "clean": True,
                    },
                    "just": {
                        "file": "Justfile",
                        "sha256": "f" * 64,
                        "git_blob": "1" * 40,
                        "recipes": ["check"],
                        "dry_run_sha256": "2" * 64,
                        "generated": True,
                    },
                    "verifier": {
                        "path": "/fixture/canonical/tasks/write-greeting/verify.sh",
                        "sha256": SOURCE_DIGEST,
                    },
                },
            )
            rows.append(
                {
                    "posture": posture,
                    "ocap": ocap,
                    "task": "write-greeting",
                    "verify": "pass",
                    "status": "completed",
                    "tool_calls": 1,
                    "write_calls": 1,
                    "total_tokens": 10,
                    "wall_secs": 1,
                    "solve_rc": 0,
                    "events_file": f"events/{cell}.jsonl",
                }
            )
    with (root / "results.csv").open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


class ValidatorFixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "run"
        self.root.mkdir()
        make_valid_fixture(self.root)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def validate(
        self, expected_mode: str = "qualification"
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "python3",
                str(VALIDATOR),
                "--expected-mode",
                expected_mode,
                str(self.root),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

    def assert_rejected(self, expected: str) -> None:
        result = self.validate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(expected, result.stdout)

    def test_complete_positive_fixture_uses_only_stable_vllm_probe(self) -> None:
        result = self.validate()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("qualification validation passed", result.stdout)

    def test_expected_mode_prevents_qualification_downgrade(self) -> None:
        manifest = self._json("manifest.json")
        manifest["mode"] = "exploratory"
        write_json(self.root / "manifest.json", manifest)
        result = self.validate(expected_mode="qualification")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("manifest mode", result.stdout)
        self.assertIn("expected 'qualification'", result.stdout)

    def test_every_retained_evidence_leaf_rejects_symlinks(self) -> None:
        relative_paths = (
            "manifest.json",
            "results.csv",
            "config.toml",
            "provenance.json",
            "harness-sources.json",
            "server-launch-manifest.json",
            "server-models.json",
            "server-version.json",
            "preflight/first-request.json",
            "preflight/second-request.json",
            "preflight/events.jsonl",
            "events/baseline-off-write-greeting.jsonl",
            "workspace-baselines/baseline-off-write-greeting.json",
        )
        pristine = Path(self.tmp.name) / "symlink-pristine"
        shutil.copytree(self.root, pristine)
        for index, relative in enumerate(relative_paths):
            with self.subTest(relative=relative):
                shutil.rmtree(self.root)
                shutil.copytree(pristine, self.root)
                path = self.root / relative
                target = self.root / f"unlisted-symlink-target-{index}"
                shutil.copyfile(path, target)
                path.unlink()
                path.symlink_to(target)
                self.assert_rejected("symlinked evidence path")

    def test_retained_evidence_rejects_symlinked_parent_and_nonregular_leaf(self) -> None:
        target = self.root / "preflight-target"
        (self.root / "preflight").rename(target)
        (self.root / "preflight").symlink_to(target, target_is_directory=True)
        self.assert_rejected("symlinked evidence path")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        (self.root / "config.toml").unlink()
        (self.root / "config.toml").mkdir()
        self.assert_rejected("regular file")

    def test_workspace_baseline_proof_is_required_and_cross_bound(self) -> None:
        path = self.root / "workspace-baselines" / "crew-off-write-greeting.json"
        path.unlink()
        self.assert_rejected("workspace baseline")

        mutations = (
            (("git", "baseline_commit"), "not-a-commit", "baseline_commit"),
            (("just", "recipes"), [], "check recipe"),
            (("just", "dry_run_sha256"), "not-a-digest", "dry-run"),
            (("verifier", "sha256"), "0" * 64, "verifier SHA-256"),
        )
        for keys, value, expected in mutations:
            with self.subTest(field=".".join(keys)):
                shutil.rmtree(self.root)
                self.root.mkdir()
                make_valid_fixture(self.root)
                path = (
                    self.root
                    / "workspace-baselines"
                    / "crew-off-write-greeting.json"
                )
                proof = json.loads(path.read_text(encoding="utf-8"))
                target = proof
                for key in keys[:-1]:
                    target = target[key]
                target[keys[-1]] = value
                write_json(path, proof)
                self.assert_rejected(expected)

    def test_existing_cell_and_contract_failures_are_rejected(self) -> None:
        mutations = {
            "failed verification": lambda: self._edit_csv("verify", "fail"),
            "nonzero solve": lambda: self._edit_csv("solve_rc", "1"),
            "missing required cell": lambda: self._drop_csv_row(),
            "zero contracts": lambda: self._drop_contracts(),
            "multiple contracts": lambda: self._duplicate_contract(),
            "mismatched effective model": lambda: self._edit_contract(
                ("effective_model",), "alias-that-was-not-requested"
            ),
            "changed newt binary": lambda: self._edit_provenance(
                ("newt", "sha256_after"), "d" * 64
            ),
            "missing preflight policy": lambda: self._drop_preflight_field(
                "parallel_tool_calls"
            ),
        }
        pristine = Path(self.tmp.name) / "pristine"
        shutil.copytree(self.root, pristine)
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                shutil.rmtree(self.root)
                shutil.copytree(pristine, self.root)
                mutate()
                result = self.validate()
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("validation failed", result.stdout)

    def test_qualification_rejects_incomplete_axes(self) -> None:
        manifest = self._json("manifest.json")
        manifest["matrix"]["postures"] = ["baseline"]
        write_json(self.root / "manifest.json", manifest)
        self.assert_rejected("complete posture set")

    def test_contract_tenacity_must_match_posture(self) -> None:
        self._edit_contract(("effective_config", "tenacity"), "standard", "tenacity", "off")
        self.assert_rejected("tenacity")

    def test_contract_cognition_and_crew_must_match_posture(self) -> None:
        self._edit_contract(
            ("effective_config", "cognition"), "off", "obsessive", "off"
        )
        self.assert_rejected("cognition")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        self._edit_contract(("effective_config", "crew"), "off", "crew", "off")
        self.assert_rejected("crew")

    def test_event_evidence_must_use_each_cells_exact_runner_owned_path(self) -> None:
        self._edit_csv_cell(
            "tenacity", "off", "events_file", "events/baseline-off-write-greeting.jsonl"
        )
        result = self.validate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("reused by", result.stdout)
        self.assertIn("expected runner-owned path", result.stdout)

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        self._edit_csv_cell(
            "tenacity", "off", "events_file", "events/some-other-evidence.jsonl"
        )
        self.assert_rejected("expected runner-owned path")

    def test_copied_event_evidence_is_rejected_by_cell_identity(self) -> None:
        source = self._event_path("baseline", "off")
        destination = self._event_path("tenacity", "off")
        records = [json.loads(line) for line in source.read_text().splitlines()]
        records[1]["effective_config"]["tenacity"] = "relentless"
        destination.write_text(
            "".join(json.dumps(record) + "\n" for record in records),
            encoding="utf-8",
        )
        self.assert_rejected("solve_result cwd")

    def test_solve_result_is_unique_and_bound_to_the_exact_cell(self) -> None:
        path = self._event_path()
        records = [json.loads(line) for line in path.read_text().splitlines()]
        records.append(dict(records[0]))
        path.write_text(
            "".join(json.dumps(record) + "\n" for record in records),
            encoding="utf-8",
        )
        self.assert_rejected("exactly one solve_result")

        for keys, value, expected in (
            (("task_file",), "/tmp/foreign-instruction.txt", "task_file"),
            (("model",), "foreign-model", "solve_result model"),
            (("backend_kind",), "ollama", "backend_kind"),
        ):
            with self.subTest(field=keys[0]):
                shutil.rmtree(self.root)
                self.root.mkdir()
                make_valid_fixture(self.root)
                self._edit_solve_result(keys, value)
                self.assert_rejected(expected)

    def test_contract_identity_and_any_extra_version_are_rejected(self) -> None:
        for keys, value, expected in (
            (("agent",), "foreign-agent", "contract agent"),
            (("agent_version",), "", "agent_version"),
            (("backend", "kind"), "ollama", "backend kind"),
        ):
            with self.subTest(field=".".join(keys)):
                shutil.rmtree(self.root)
                self.root.mkdir()
                make_valid_fixture(self.root)
                self._edit_contract(keys, value)
                self.assert_rejected(expected)

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        path = self._event_path()
        path.write_text(
            path.read_text(encoding="utf-8")
            + json.dumps({"contract_version": "2", "agent": "foreign-agent"})
            + "\n",
            encoding="utf-8",
        )
        self.assert_rejected("exactly one contract record")

    def test_config_must_be_the_exact_generated_toml(self) -> None:
        path = self.root / "config.toml"
        path.write_text(
            "".join(f"# {line}\n" for line in path.read_text().splitlines()),
            encoding="utf-8",
        )
        self.assert_rejected("config.toml")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        path = self.root / "config.toml"
        path.write_text(path.read_text() + "unexpected = true\n", encoding="utf-8")
        self.assert_rejected("config.toml")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        path = self.root / "preflight" / "config.toml"
        path.write_text(
            path.read_text().replace("http://127.0.0.1:43123", "http://foreign.invalid"),
            encoding="utf-8",
        )
        self.assert_rejected("preflight/config.toml")

    def test_preflight_events_prove_bounded_continuation_and_exact_identity(self) -> None:
        path = self.root / "preflight" / "events.jsonl"
        records = [json.loads(line) for line in path.read_text().splitlines()]
        records[0]["cwd"] = str((self.root / "ws" / "foreign").resolve())
        path.write_text(
            "".join(json.dumps(record) + "\n" for record in records),
            encoding="utf-8",
        )
        self.assert_rejected("preflight: solve_result cwd")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        path = self.root / "preflight" / "events.jsonl"
        records = [json.loads(line) for line in path.read_text().splitlines()]
        records[-1]["continuation_succeeded"] = False
        path.write_text(
            "".join(json.dumps(record) + "\n" for record in records),
            encoding="utf-8",
        )
        self.assert_rejected("reasoning_overflow")

        shutil.rmtree(self.root)
        self.root.mkdir()
        make_valid_fixture(self.root)
        path = self.root / "preflight" / "events.jsonl"
        path.write_text(
            path.read_text()
            + json.dumps({"kind": "solve_result", "status": "completed"})
            + "\n",
            encoding="utf-8",
        )
        self.assert_rejected("exactly one solve_result")

    def test_preflight_second_request_must_be_retained(self) -> None:
        (self.root / "preflight" / "second-request.json").unlink()
        self.assert_rejected("preflight/second-request.json")

    def test_preflight_second_request_must_prove_reasoning_replay(self) -> None:
        path = self.root / "preflight" / "second-request.json"
        request = self._json("preflight/second-request.json")
        request["messages"][-1]["reasoning_content"] = "copied-or-missing"
        write_json(path, request)
        self.assert_rejected("reasoning replay marker")

    def test_junk_preflight_events_are_not_accepted_as_evidence(self) -> None:
        (self.root / "preflight" / "events.jsonl").write_text(
            json.dumps({"unrelated": "junk"}) + "\n", encoding="utf-8"
        )
        result = self.validate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        for expected in (
            "exactly one solve_result",
            "exactly one contract record",
            "exactly one reasoning_overflow",
        ):
            self.assertIn(expected, result.stdout)

    def test_source_manifest_lists_every_required_task_source(self) -> None:
        sources = self._json("harness-sources.json")
        sources["files"] = [
            entry
            for entry in sources["files"]
            if entry["path"] != "tasks/write-greeting/verify.sh"
        ]
        self._rewrite_source_identity(sources)
        self.assert_rejected("tasks/write-greeting/verify.sh")

    def test_source_manifest_lists_discoverable_task_setup(self) -> None:
        tasks_dir = (self.root / "fixture-tasks").resolve()
        task_dir = tasks_dir / "write-greeting"
        task_dir.mkdir(parents=True)
        for name in ("instruction.txt", "verify.sh", "setup.sh"):
            (task_dir / name).write_text("fixture\n", encoding="utf-8")
        manifest = self._json("manifest.json")
        manifest["matrix"]["tasks_dir"] = str(tasks_dir)
        write_json(self.root / "manifest.json", manifest)
        for path in (self.root / "events").glob("*.jsonl"):
            records = [json.loads(line) for line in path.read_text().splitlines()]
            records[0]["task_file"] = str((task_dir / "instruction.txt").resolve())
            path.write_text(
                "".join(json.dumps(record) + "\n" for record in records),
                encoding="utf-8",
            )
        self.assert_rejected("tasks/write-greeting/setup.sh")

    def test_launch_manifest_must_match_and_retain_its_hash(self) -> None:
        launch = self._json("server-launch-manifest.json")
        launch["chat_template_id"] = "different-template"
        write_json(self.root / "server-launch-manifest.json", launch)
        self.assert_rejected("launch manifest")

    def test_launch_manifest_recorded_hash_must_match_retained_bytes(self) -> None:
        self._edit_provenance(
            ("server", "launch_manifest", "sha256"), "0" * 64
        )
        self.assert_rejected("launch manifest SHA-256")

    def test_models_probe_must_prove_requested_model_is_served(self) -> None:
        write_json(
            self.root / "server-models.json",
            {"object": "list", "data": [{"id": "some-other-model"}]},
        )
        self.assert_rejected("does not serve requested model")

    def test_model_digest_must_be_an_immutable_revision_or_sha256(self) -> None:
        manifest = self._json("manifest.json")
        manifest["model"]["digest"] = "friendly-but-mutable-label"
        write_json(self.root / "manifest.json", manifest)
        self.assert_rejected("model digest must be")

    def test_model_digest_accepts_revision_and_prefixed_sha256_shapes(self) -> None:
        for digest in ("d" * 40, "sha256:" + "d" * 64):
            with self.subTest(digest=digest):
                shutil.rmtree(self.root)
                self.root.mkdir()
                make_valid_fixture(self.root)
                self._replace_model_digest(digest)
                result = self.validate()
                self.assertEqual(result.returncode, 0, result.stdout)

    def test_vllm_version_probe_must_equal_declared_version(self) -> None:
        write_json(self.root / "server-version.json", {"version": "0.18.0"})
        self.assert_rejected("vLLM version")

    def test_llama_props_must_ground_build_context_and_template(self) -> None:
        manifest = self._json("manifest.json")
        provenance = self._json("provenance.json")
        launch = self._json("server-launch-manifest.json")
        for value in (manifest["server"], provenance["server"]):
            value["kind"] = "llama_cpp"
            value["version"] = "b6000"
            value["chat_template_id"] = "sha256:" + hashlib.sha256(
                b"fixture-template"
            ).hexdigest()
            value["tool_parser_id"] = "builtin:qwen3"
            value["reasoning_parser_id"] = "builtin:nemotron"
        launch.update(
            {
                "server_kind": "llama_cpp",
                "server_version": "b6000",
                "chat_template_id": manifest["server"]["chat_template_id"],
                "tool_parser_id": "builtin:qwen3",
                "reasoning_parser_id": "builtin:nemotron",
            }
        )
        write_json(self.root / "server-launch-manifest.json", launch)
        launch_digest = sha256(self.root / "server-launch-manifest.json")
        manifest["server"]["launch_manifest_sha256"] = launch_digest
        provenance["server"]["launch_manifest"]["sha256"] = launch_digest
        write_json(self.root / "manifest.json", manifest)
        write_json(self.root / "provenance.json", provenance)
        (self.root / "server-version.json").unlink()
        write_json(
            self.root / "server-props.json",
            {
                "build_info": "b6000",
                "default_generation_settings": {"n_ctx": 32768},
                "chat_template": "fixture-template",
            },
        )
        self.assert_rejected("n_ctx")

    def test_harness_source_hash_must_be_unchanged(self) -> None:
        sources = self._json("harness-sources.json")
        sources["files"][0]["sha256_after"] = "f" * 64
        sources["aggregate_sha256_after"] = source_aggregate(
            [(sources["files"][0]["path"], "f" * 64)]
        )
        sources["differences"] = ["changed:harness/run-matrix.sh"]
        write_json(self.root / "harness-sources.json", sources)
        provenance = self._json("provenance.json")
        provenance["harness"]["source_sha256_after"] = sources[
            "aggregate_sha256_after"
        ]
        write_json(self.root / "provenance.json", provenance)
        self.assert_rejected("harness source")

    def _json(self, relative: str) -> dict:
        return json.loads((self.root / relative).read_text(encoding="utf-8"))

    def _edit_csv(self, key: str, value: str) -> None:
        path = self.root / "results.csv"
        with path.open(encoding="utf-8") as f:
            rows = list(csv.DictReader(f))
        rows[0][key] = value
        with path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)

    def _edit_csv_cell(
        self, posture: str, ocap: str, key: str, value: str
    ) -> None:
        path = self.root / "results.csv"
        with path.open(encoding="utf-8") as f:
            rows = list(csv.DictReader(f))
        row = next(
            row
            for row in rows
            if row["posture"] == posture and row["ocap"] == ocap
        )
        row[key] = value
        with path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)

    def _drop_csv_row(self) -> None:
        path = self.root / "results.csv"
        with path.open(encoding="utf-8") as f:
            rows = list(csv.DictReader(f))
        with path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows[1:])

    def _event_path(self, posture: str = "baseline", ocap: str = "off") -> Path:
        return self.root / "events" / f"{cell_name(posture, ocap)}.jsonl"

    def _drop_contracts(self) -> None:
        path = self._event_path()
        first = path.read_text(encoding="utf-8").splitlines()[0]
        path.write_text(first + "\n", encoding="utf-8")

    def _duplicate_contract(self) -> None:
        path = self._event_path()
        lines = path.read_text(encoding="utf-8").splitlines()
        path.write_text("\n".join(lines + [lines[1]]) + "\n", encoding="utf-8")

    def _edit_contract(
        self,
        keys: tuple[str, ...],
        value: object,
        posture: str = "baseline",
        ocap: str = "off",
    ) -> None:
        path = self._event_path(posture, ocap)
        lines = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
        target = lines[1]
        for key in keys[:-1]:
            target = target[key]
        target[keys[-1]] = value
        path.write_text(
            "".join(json.dumps(line) + "\n" for line in lines), encoding="utf-8"
        )

    def _edit_solve_result(
        self,
        keys: tuple[str, ...],
        value: object,
        posture: str = "baseline",
        ocap: str = "off",
    ) -> None:
        path = self._event_path(posture, ocap)
        lines = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
        target = lines[0]
        for key in keys[:-1]:
            target = target[key]
        target[keys[-1]] = value
        path.write_text(
            "".join(json.dumps(line) + "\n" for line in lines), encoding="utf-8"
        )

    def _rewrite_source_identity(self, sources: dict) -> None:
        entries = [
            (entry["path"], entry["sha256_before"])
            for entry in sources["files"]
        ]
        aggregate = source_aggregate(entries)
        sources["aggregate_sha256_before"] = aggregate
        sources["aggregate_sha256_after"] = aggregate
        write_json(self.root / "harness-sources.json", sources)
        provenance = self._json("provenance.json")
        provenance["harness"]["source_sha256_before"] = aggregate
        provenance["harness"]["source_sha256_after"] = aggregate
        write_json(self.root / "provenance.json", provenance)

    def _replace_model_digest(self, digest: str) -> None:
        manifest = self._json("manifest.json")
        provenance = self._json("provenance.json")
        launch = self._json("server-launch-manifest.json")
        manifest["model"]["digest"] = digest
        provenance["model"]["digest"] = digest
        launch["model_digest"] = digest
        write_json(self.root / "server-launch-manifest.json", launch)
        launch_digest = sha256(self.root / "server-launch-manifest.json")
        manifest["server"]["launch_manifest_sha256"] = launch_digest
        provenance["server"]["launch_manifest"]["sha256"] = launch_digest
        write_json(self.root / "manifest.json", manifest)
        write_json(self.root / "provenance.json", provenance)
        for path in [
            *(self.root / "events").glob("*.jsonl"),
            self.root / "preflight" / "events.jsonl",
        ]:
            records = [json.loads(line) for line in path.read_text().splitlines()]
            for record in records:
                if "contract_version" in record:
                    record["model_digest"] = digest
            path.write_text(
                "".join(json.dumps(record) + "\n" for record in records),
                encoding="utf-8",
            )

    def _edit_provenance(self, keys: tuple[str, ...], value: object) -> None:
        path = self.root / "provenance.json"
        data = self._json("provenance.json")
        target = data
        for key in keys[:-1]:
            target = target[key]
        target[keys[-1]] = value
        write_json(path, data)

    def _drop_preflight_field(self, key: str) -> None:
        path = self.root / "preflight" / "first-request.json"
        data = json.loads(path.read_text(encoding="utf-8"))
        del data[key]
        write_json(path, data)


if __name__ == "__main__":
    unittest.main()
