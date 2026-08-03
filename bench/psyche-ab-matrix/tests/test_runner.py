#!/usr/bin/env python3
"""Offline end-to-end BAT for the matrix collector and final gate."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Optional


HERE = Path(__file__).resolve().parent
RUNNER = HERE.parent / "run-matrix.sh"
VALIDATOR = HERE.parent / "validate-run.py"
REPO = HERE.parents[2]
MODEL = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16"


FAKE_NEWT = r'''#!/usr/bin/env python3
import json, os, pathlib, re, sys, urllib.request
args = sys.argv[1:]
if args == ["--version"]:
    print("newt 0.7.6 (offline fixture)")
    raise SystemExit(0)
def option(name, default=None):
    return args[args.index(name) + 1] if name in args else default
config = pathlib.Path(option("--config")).read_text(encoding="utf-8")
def setting(name):
    return json.loads(re.search(r"^" + re.escape(name) + r" = (.+)$", config, re.M).group(1))
endpoint, model, backend = setting("endpoint"), setting("model"), setting("name")
events = pathlib.Path(option("--events"))
cwd = pathlib.Path(option("--cwd"))
digest = option("--model-digest")
context = int(option("--context-window"))
rounds = int(option("--max-rounds"))
if endpoint.startswith("http://127.0.0.1:"):
    post_count = int(os.environ.get("FAKE_PREFLIGHT_POSTS", "2"))
    body = {
        "model": model,
        "messages": [{"role": "user", "content": "Answer briefly that the qualification preflight is complete. Do not call a tool."}],
        "tools": [{"type": "function", "function": {"name": "list_dir", "parameters": {"type": "object", "properties": {}}}}],
        "tool_choice": "auto",
        "max_tokens": 16000,
        "temperature": 0.6,
        "top_p": 0.95,
        "parallel_tool_calls": False,
        "chat_template_kwargs": {"enable_thinking": True, "truncate_history_thinking": True},
    }
    if post_count >= 1:
        request = urllib.request.Request(
            endpoint + "/v1/chat/completions",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=2) as response:
            first_response = json.load(response)
        reasoning = first_response["choices"][0]["message"]["reasoning_content"]
    if post_count >= 2:
        second_body = dict(body)
        second_body["messages"] = body["messages"] + [
            {"role": "assistant", "content": None, "reasoning_content": reasoning}
        ]
        second_request = urllib.request.Request(
            endpoint + "/v1/chat/completions",
            data=json.dumps(second_body).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(second_request, timeout=2) as response:
            json.load(response)
else:
    (cwd / "greeting.txt").write_text("hello\n", encoding="utf-8")
    if os.environ.get("FAKE_MANIFEST_MODE"):
        manifest_path = events.parents[1] / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["mode"] = os.environ["FAKE_MANIFEST_MODE"]
        manifest_path.write_text(json.dumps(manifest) + "\n", encoding="utf-8")
effective_model = model if endpoint.startswith("http://127.0.0.1:") else os.environ.get("FAKE_EFFECTIVE_MODEL", model)
tenacity = "relentless" if "--tenacity" in args or "--obsessive" in args else "standard"
cognition = option("--cognition", "contemplating" if "--obsessive" in args else "default")
crew = "on" if os.environ.get("NEWT_TEAM") is not None or "--obsessive" in args else "off"
records = [
    {"kind": "solve_result", "task_file": str(pathlib.Path(option("--instruction-file")).resolve()),
     "cwd": str(cwd.resolve()), "model": model, "backend_kind": "openai",
     "status": "completed", "tool_calls": 1,
     "write_calls": 1, "usage_total_tokens": 10, "wall_secs": 0.1},
    {"contract_version": "1", "requested_model": model, "effective_model": effective_model,
     "model_digest": digest, "outcome": "completed", "backend": {"name": backend, "kind": "openai"},
     "agent": "newt-agent", "agent_version": "0.7.6",
     "effective_config": {"context_window": context, "tenacity": tenacity,
                          "cognition": cognition, "crew": crew,
                          "ocap": "on" if "--confined" in args else "off", "max_rounds": rounds},
     "timing": {"wall_ms": 100}},
]
if endpoint.startswith("http://127.0.0.1:"):
    records.append(
        {"kind": "reasoning_overflow", "round": 0,
         "reasoning_overflow_detected": True, "continuation_attempted": True,
         "continuation_succeeded": True, "finish_reason": "length",
         "reasoning_tokens_estimate": 1}
    )
events.parent.mkdir(parents=True, exist_ok=True)
events.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
for record in records:
    print(json.dumps(record))
'''


FAKE_CURL = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if os.environ.get("FAKE_CURL_ARGV_LOG"):
    with open(os.environ["FAKE_CURL_ARGV_LOG"], "a", encoding="utf-8") as log:
        log.write(json.dumps(args) + "\n")
if "@-" in args:
    header = sys.stdin.read()
    if not header.startswith("Authorization: Bearer "):
        raise SystemExit("missing bearer header on stdin")
output = pathlib.Path(args[args.index("-o") + 1])
url = next(arg for arg in args if arg.startswith("http"))
if url.endswith("/version"):
    value = {"version": "0.19.0"}
elif url.endswith("/props"):
    expected = "model=" + os.environ["MODEL_ID"]
    if "--get" not in args or expected not in args:
        raise SystemExit("llama.cpp /props probe did not select MODEL_ID")
    value = {
        "build_info": "b6000",
        "default_generation_settings": {"n_ctx": 65536},
        "chat_template": "fixture-template",
    }
else:
    value = {"object": "list", "data": [{"id": os.environ["MODEL_ID"]}]}
output.write_text(json.dumps(value) + "\n", encoding="utf-8")
'''


FAKE_TIMEOUT = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_TIMEOUT_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\n")
if not args or not args[0].startswith("--kill-after="):
    raise SystemExit("timeout invocation lacks --kill-after")
args = args[1:]
if not args:
    raise SystemExit("timeout invocation lacks duration")
args = args[1:]
os.execvp(args[0], args)
'''


class RunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        self.newt = bin_dir / "newt"
        self.newt.write_text(FAKE_NEWT, encoding="utf-8")
        self.newt.chmod(0o755)
        curl = bin_dir / "curl"
        curl.write_text(FAKE_CURL, encoding="utf-8")
        curl.chmod(0o755)
        timeout = bin_dir / "timeout"
        timeout.write_text(FAKE_TIMEOUT, encoding="utf-8")
        timeout.chmod(0o755)
        self.timeout_log = self.root / "timeout-argv.jsonl"
        self.tasks = self.root / "tasks" / "write-greeting"
        self.tasks.mkdir(parents=True)
        (self.tasks / "instruction.txt").write_text("Write greeting.txt\n", encoding="utf-8")
        verify = self.tasks / "verify.sh"
        verify.write_text("#!/bin/sh\n[ -s greeting.txt ]\n", encoding="utf-8")
        verify.chmod(0o755)
        self.launch_manifest = self.root / "server-launch.json"
        self.launch_manifest.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "model_id": MODEL,
                    "model_digest": "b" * 64,
                    "context_window": 65536,
                    "server_kind": "vllm",
                    "server_version": "0.19.0",
                    "chat_template_id": "nemotron-v3@fixture",
                    "tool_parser_id": "qwen3_coder",
                    "reasoning_parser_id": "nano_v3",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.env = os.environ.copy()
        self.env.update(
            {
                "PATH": str(bin_dir) + os.pathsep + self.env["PATH"],
                "PY": sys.executable,
                "NEWT": str(self.newt),
                "MODEL_ENDPOINT": "http://fixture.invalid:8000",
                "MODEL_ID": MODEL,
                "MODEL_DIGEST": "b" * 64,
                "CONTEXT_WINDOW": "65536",
                "SERVER_KIND": "vllm",
                "SERVER_VERSION": "0.19.0",
                "CHAT_TEMPLATE_ID": "nemotron-v3@fixture",
                "TOOL_PARSER_ID": "qwen3_coder",
                "REASONING_PARSER_ID": "nano_v3",
                "SERVER_LAUNCH_MANIFEST": str(self.launch_manifest),
                "TASKS_DIR": str(self.tasks.parent / ".." / "tasks"),
                "POSTURES": "baseline tenacity crew obsessive",
                "OCAP_MODES": "off on",
                "TASK_TIMEOUT": "5",
                "FAKE_TIMEOUT_LOG": str(self.timeout_log),
                # These fixtures exercise the RUN + dirty-recording paths, not the
                # clean-tree gate, and the dev/CI checkout may itself be dirty —
                # so opt into the dirty override. The gate itself is covered by
                # test_qualification_* below.
                "ALLOW_DIRTY_QUALIFICATION": "1",
            }
        )

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def run_fixture(
        self, name: str, effective_model: Optional[str] = None
    ) -> subprocess.CompletedProcess[str]:
        env = self.env.copy()
        env["OUT"] = str(self.root / name)
        if effective_model:
            env["FAKE_EFFECTIVE_MODEL"] = effective_model
        return subprocess.run(
            ["bash", str(RUNNER)],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=30,
        )

    def _run_env(self, name: str, **overrides: object) -> subprocess.CompletedProcess[str]:
        env = self.env.copy()
        env["OUT"] = str(self.root / name)
        for key, value in overrides.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = str(value)
        return subprocess.run(
            ["bash", str(RUNNER)],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=30,
        )

    def test_qualification_rejects_a_dirty_tree_without_override(self) -> None:
        # B3: a dirty source tree in qualification mode fails closed BEFORE any
        # inference — the bundle would otherwise pin a commit + binary hash for
        # something git cannot reconstruct.
        result = self._run_env(
            "dirty-reject", FAKE_SOURCE_DIRTY="true", ALLOW_DIRTY_QUALIFICATION=None
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("clean source tree", result.stdout)
        # It bailed early: no results were produced.
        self.assertFalse((self.root / "dirty-reject" / "results.csv").is_file())

    def test_dirty_override_runs_and_retains_the_diff(self) -> None:
        # B3: the explicit override still runs, but retains the diff + untracked
        # listing so the run is reproducible/inspectable.
        result = self._run_env(
            "dirty-override", FAKE_SOURCE_DIRTY="true", ALLOW_DIRTY_QUALIFICATION="1"
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        out = self.root / "dirty-override"
        self.assertTrue((out / "source-dirty.patch").is_file())
        self.assertTrue((out / "source-dirty-status.txt").is_file())
        provenance = json.loads((out / "provenance.json").read_text(encoding="utf-8"))
        self.assertTrue(provenance["harness"]["dirty"])

    def _revalidate(self, out: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(VALIDATOR), "--expected-mode", "qualification", str(out)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=60,
        )

    def test_validation_is_hermetic_against_task_tree_changes(self) -> None:
        # B2: revalidation must depend only on the retained bundle, never the live
        # task tree. The fixture task has no setup.sh, so the manifest pins
        # tasks_with_setup = []. Adding a setup.sh to the live tree afterward — the
        # exact mutation the old `is_file()` check reacted to — must NOT change the
        # verdict, and neither must deleting the tree entirely.
        result = self.run_fixture("hermetic")
        self.assertEqual(result.returncode, 0, result.stdout)
        out = self.root / "hermetic"
        manifest = json.loads((out / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["matrix"]["tasks_with_setup"], [])
        tasks_dir = Path(manifest["matrix"]["tasks_dir"])
        # Mutate the live tree: add a setup.sh the pinned bundle never captured.
        for task in manifest["matrix"]["tasks"]:
            (tasks_dir / task / "setup.sh").write_text("#!/bin/sh\n", encoding="utf-8")
        self.assertEqual(self._revalidate(out).returncode, 0, "added setup.sh must not matter")
        # And it still validates after the original tree is deleted entirely.
        shutil.rmtree(tasks_dir)
        self.assertEqual(self._revalidate(out).returncode, 0, "deleted tree must not matter")

    def test_complete_fixture_passes_and_retains_raw_artifacts(self) -> None:
        expected_dirty = bool(
            subprocess.run(
                ["git", "-C", str(REPO), "status", "--porcelain", "--untracked-files=normal"],
                text=True,
                stdout=subprocess.PIPE,
                check=True,
            ).stdout
        )
        result = self.run_fixture("valid")
        self.assertEqual(result.returncode, 0, result.stdout)
        out = self.root / "valid"
        self.assertIn("qualification validation passed", result.stdout)
        self.assertTrue((out / "preflight" / "first-request.json").is_file())
        self.assertTrue((out / "preflight" / "second-request.json").is_file())
        self.assertTrue((out / "events" / "baseline-off-write-greeting.jsonl").is_file())
        self.assertTrue((out / "server-launch-manifest.json").is_file())
        self.assertTrue((out / "harness-sources.json").is_file())
        manifest = json.loads((out / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["matrix"]["tasks_dir"], str(self.tasks.parent.resolve()))
        provenance = json.loads((out / "provenance.json").read_text(encoding="utf-8"))
        self.assertEqual(provenance["harness"]["dirty"], expected_dirty)
        self.assertIn("**PASS**", (out / "matrix.md").read_text(encoding="utf-8"))
        for posture in ("baseline", "tenacity", "crew", "obsessive"):
            for ocap in ("off", "on"):
                workspace = out / "ws" / f"{posture}-{ocap}-write-greeting"
                git_root = subprocess.run(
                    ["git", "-C", str(workspace), "rev-parse", "--show-toplevel"],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=True,
                ).stdout.strip()
                self.assertEqual(git_root, str(workspace.resolve()))
                subprocess.run(
                    ["git", "-C", str(workspace), "rev-parse", "--verify", "HEAD"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=True,
                )
                subprocess.run(
                    ["just", "--justfile", str(workspace / "Justfile"), "check"],
                    cwd=workspace,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=True,
                )
                committed = subprocess.run(
                    ["git", "-C", str(workspace), "show", "HEAD:Justfile"],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=True,
                ).stdout
                self.assertIn("check:", committed)
                proof_path = out / "workspace-baselines" / f"{posture}-{ocap}-write-greeting.json"
                proof = json.loads(proof_path.read_text(encoding="utf-8"))
                self.assertEqual(proof["cell"], f"{posture}-{ocap}-write-greeting")
                self.assertEqual(proof["workspace"], str(workspace.resolve()))
                self.assertRegex(proof["git"]["baseline_commit"], r"^[0-9a-f]{40}$")
                self.assertTrue(proof["git"]["clean"])
                self.assertIn("check", proof["just"]["recipes"])
                self.assertRegex(proof["just"]["dry_run_sha256"], r"^[0-9a-f]{64}$")

        timeout_calls = [
            json.loads(line)
            for line in self.timeout_log.read_text(encoding="utf-8").splitlines()
        ]
        self.assertTrue(timeout_calls)
        self.assertTrue(
            all(call[0] == "--kill-after=5s" for call in timeout_calls),
            timeout_calls,
        )

        # Final validation relies on retained baseline proof, not model-mutable
        # workspace metadata after the cell has completed.
        mutable_workspace = out / "ws" / "crew-off-write-greeting"
        shutil.rmtree(mutable_workspace / ".git")
        (mutable_workspace / "Justfile").unlink()
        revalidated = subprocess.run(
            [
                sys.executable,
                str(VALIDATOR),
                "--expected-mode",
                "qualification",
                str(out),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        self.assertEqual(revalidated.returncode, 0, revalidated.stdout)

    def test_existing_project_markers_are_preserved_in_committed_workspaces(self) -> None:
        setup = self.tasks / "setup.sh"
        setup.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' '[package]' 'name = \"fixture\"' > Cargo.toml\n"
            "printf '%s\\n' '# keep-this-marker' 'check:' "
            f"'\t@{self.tasks / 'verify.sh'}' > Justfile\n",
            encoding="utf-8",
        )
        setup.chmod(0o755)
        result = self.run_fixture("existing-marker")
        self.assertEqual(result.returncode, 0, result.stdout)
        workspace = self.root / "existing-marker" / "ws" / "crew-off-write-greeting"
        self.assertIn(
            "keep-this-marker", (workspace / "Justfile").read_text(encoding="utf-8")
        )
        self.assertEqual(
            (workspace / "Cargo.toml").read_text(encoding="utf-8"),
            '[package]\nname = "fixture"\n',
        )
        tracked = subprocess.run(
            ["git", "-C", str(workspace), "show", "HEAD:Cargo.toml"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=True,
        ).stdout
        self.assertEqual(tracked, '[package]\nname = "fixture"\n')

    def test_symlinked_task_directories_and_required_files_are_rejected(self) -> None:
        external = self.root / "external-task"
        shutil.copytree(self.tasks, external)
        linked = self.tasks.parent / "linked-task"
        linked.symlink_to(external, target_is_directory=True)
        result = self.run_fixture("symlink-directory")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("symlinked task", result.stdout)
        linked.unlink()

        originals = {
            "instruction.txt": ("Write greeting.txt\n", 0o644),
            "verify.sh": ("#!/bin/sh\n[ -s greeting.txt ]\n", 0o755),
            "setup.sh": ("#!/bin/sh\n:\n", 0o755),
        }
        for filename, (content, mode) in originals.items():
            with self.subTest(filename=filename):
                path = self.tasks / filename
                if path.exists() or path.is_symlink():
                    path.unlink()
                target = self.root / f"external-{filename}"
                target.write_text(content, encoding="utf-8")
                target.chmod(mode)
                path.symlink_to(target)
                result = self.run_fixture(f"symlink-{filename}")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"symlinked {filename}", result.stdout)
                path.unlink()
                if filename != "setup.sh":
                    path.write_text(content, encoding="utf-8")
                    path.chmod(mode)

    def test_model_digest_rejects_mutable_or_ambiguous_identifiers(self) -> None:
        self.env["MODEL_DIGEST"] = "latest"
        result = self.run_fixture("invalid-digest")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("40 hex", result.stdout)
        self.assertIn("sha256:<64 hex>", result.stdout)

    def test_model_digest_accepts_each_documented_immutable_form(self) -> None:
        for name, digest in (
            ("revision-40", "a" * 40),
            ("raw-64", "b" * 64),
            ("sha256-64", "sha256:" + "c" * 64),
        ):
            with self.subTest(name=name):
                self.env["MODEL_DIGEST"] = digest
                launch = json.loads(self.launch_manifest.read_text(encoding="utf-8"))
                launch["model_digest"] = digest
                self.launch_manifest.write_text(
                    json.dumps(launch) + "\n", encoding="utf-8"
                )
                result = self.run_fixture(f"digest-{name}")
                self.assertEqual(result.returncode, 0, result.stdout)

    def test_probe_bearer_token_is_neither_in_curl_argv_nor_retained(self) -> None:
        secret = "qualification-secret-never-retain"
        key_file = self.root / "model.key"
        key_file.write_text(secret + "\n", encoding="utf-8")
        argv_log = self.root / "curl-argv.jsonl"
        self.env.update(
            {"MODEL_KEY_FILE": str(key_file), "FAKE_CURL_ARGV_LOG": str(argv_log)}
        )
        result = self.run_fixture("authenticated")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn(secret, argv_log.read_text(encoding="utf-8"))
        for path in (self.root / "authenticated").rglob("*"):
            if path.is_file():
                self.assertNotIn(secret.encode(), path.read_bytes(), str(path))

    def test_model_mismatch_renders_complete_report_then_exits_nonzero(self) -> None:
        result = self.run_fixture("bad-model", "wrong-served-model")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        out = self.root / "bad-model"
        self.assertIn("effective_model", (out / "validation.txt").read_text(encoding="utf-8"))
        report = (out / "matrix.md").read_text(encoding="utf-8")
        self.assertIn("Per-cell task detail", report)
        self.assertIn("**FAIL**", report)

    def test_runner_binds_validation_to_original_mode(self) -> None:
        self.env["FAKE_MANIFEST_MODE"] = "exploratory"
        result = self.run_fixture("mode-downgrade")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        validation = (self.root / "mode-downgrade" / "validation.txt").read_text(
            encoding="utf-8"
        )
        self.assertIn("expected 'qualification'", validation)

    def test_incomplete_preflight_cannot_leave_runner_waiting(self) -> None:
        for post_count in (0, 1):
            with self.subTest(post_count=post_count):
                self.env["FAKE_PREFLIGHT_POSTS"] = str(post_count)
                started = time.monotonic()
                result = self.run_fixture(f"preflight-posts-{post_count}")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("did not capture exactly two requests", result.stdout)
                self.assertLess(time.monotonic() - started, 5)

    def test_qualification_rejects_subset_axes_before_running(self) -> None:
        self.env["POSTURES"] = "baseline"
        result = self.run_fixture("subset")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("complete posture set", result.stdout)

    def test_git_and_just_are_checked_as_upfront_prerequisites(self) -> None:
        available = {
            name: shutil.which(name)
            for name in ("bash", "dirname", "curl", "timeout", "git", "just")
        }
        self.assertTrue(all(available.values()), available)
        for missing in ("git", "just"):
            with self.subTest(missing=missing):
                path_dir = self.root / f"path-without-{missing}"
                path_dir.mkdir()
                for name, source in available.items():
                    if name != missing:
                        (path_dir / name).symlink_to(source)
                env = self.env.copy()
                env["PATH"] = str(path_dir)
                env["OUT"] = str(self.root / f"missing-{missing}")
                result = subprocess.run(
                    ["bash", str(RUNNER)],
                    env=env,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                    timeout=10,
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"need {missing} on PATH", result.stdout)
                self.assertFalse((self.root / f"missing-{missing}").exists())

    def test_qualification_rejects_launch_manifest_env_mismatch(self) -> None:
        launch = json.loads(self.launch_manifest.read_text(encoding="utf-8"))
        launch["model_digest"] = "wrong-digest"
        self.launch_manifest.write_text(json.dumps(launch) + "\n", encoding="utf-8")
        result = self.run_fixture("launch-mismatch")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("server launch manifest model_digest", result.stdout)
        self.assertFalse(
            (self.root / "launch-mismatch" / "preflight" / "first-request.json").exists()
        )

    def test_llama_lane_binds_props_to_model_build_context_and_template(self) -> None:
        template_id = "sha256:" + hashlib.sha256(b"fixture-template").hexdigest()
        self.env.update(
            {
                "SERVER_KIND": "llama_cpp",
                "SERVER_VERSION": "b6000",
                "CHAT_TEMPLATE_ID": template_id,
                "TOOL_PARSER_ID": "builtin:qwen3",
                "REASONING_PARSER_ID": "builtin:nemotron",
            }
        )
        launch = json.loads(self.launch_manifest.read_text(encoding="utf-8"))
        launch.update(
            {
                "server_kind": "llama_cpp",
                "server_version": "b6000",
                "chat_template_id": template_id,
                "tool_parser_id": "builtin:qwen3",
                "reasoning_parser_id": "builtin:nemotron",
            }
        )
        self.launch_manifest.write_text(json.dumps(launch) + "\n", encoding="utf-8")

        result = self.run_fixture("llama-valid")
        self.assertEqual(result.returncode, 0, result.stdout)
        props = json.loads(
            (self.root / "llama-valid" / "server-props.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(props["build_info"], "b6000")

    def test_default_run_bundles_are_git_ignored_as_sensitive(self) -> None:
        for directory in ("runs", "qualification-runs"):
            result = subprocess.run(
                [
                    "git",
                    "-C",
                    str(REPO),
                    "check-ignore",
                    "-q",
                    f"bench/psyche-ab-matrix/{directory}/fixture/events.jsonl",
                ],
                check=False,
            )
            self.assertEqual(result.returncode, 0, directory)


if __name__ == "__main__":
    unittest.main()
