#!/usr/bin/env python3
"""Fail-closed validator for a collected Psyche qualification matrix."""

from __future__ import annotations

import argparse
import csv
import itertools
import json
import re
import stat
import sys
from pathlib import Path
from typing import Any

from qualification_harness import (
    HARNESS_FILES,
    aggregate_source_entries,
    first_request_errors,
    generated_config_errors,
    launch_manifest_errors,
    models_response_errors,
    second_request_errors,
    server_probe_errors,
    sha256_file,
)


REQUIRED_CSV_FIELDS = {
    "posture",
    "ocap",
    "task",
    "verify",
    "status",
    "solve_rc",
    "events_file",
}
SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-fA-F]{40}$")
OBJECT_ID_RE = re.compile(r"^[0-9a-fA-F]{40}(?:[0-9a-fA-F]{24})?$")
MODEL_DIGEST_RE = re.compile(
    r"^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64}|sha256:[0-9a-fA-F]{64})$"
)
QUALIFICATION_POSTURES = {"baseline", "tenacity", "crew", "obsessive"}
QUALIFICATION_OCAPS = {"off", "on"}
POSTURE_TENACITY = {
    "baseline": "standard",
    "tenacity": "relentless",
    "crew": "standard",
    "obsessive": "relentless",
}
POSTURE_COGNITION = {
    "baseline": "default",
    "tenacity": "default",
    "crew": "default",
    "obsessive": "contemplating",
}
POSTURE_CREW = {
    "baseline": "off",
    "tenacity": "off",
    "crew": "on",
    "obsessive": "on",
}


class RunValidator:
    def __init__(self, root: Path, expected_mode: str) -> None:
        self.root = root.resolve()
        self.errors: list[str] = []
        self.expected_mode = expected_mode
        self.mode = expected_mode
        self.baseline_verifier_hashes: dict[tuple[str, str, str], str] = {}

    def error(self, message: str) -> None:
        self.errors.append(message)

    def evidence_file(self, relative: str) -> Path | None:
        """Resolve one runner-owned artifact without following any symlink."""
        relative_path = Path(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts:
            self.error(f"evidence path escapes the run directory: {relative}")
            return None
        candidate = self.root / relative_path
        current = self.root
        for index, part in enumerate(relative_path.parts):
            current /= part
            try:
                mode = current.lstat().st_mode
            except OSError:
                self.error(f"missing required artifact: {relative}")
                return None
            if stat.S_ISLNK(mode):
                self.error(f"symlinked evidence path is not allowed: {relative}")
                return None
            if index < len(relative_path.parts) - 1 and not stat.S_ISDIR(mode):
                self.error(f"evidence path component is not a directory: {relative}")
                return None
        try:
            candidate.resolve(strict=True).relative_to(self.root)
        except (OSError, ValueError):
            self.error(f"evidence path escapes the run directory: {relative}")
            return None
        try:
            if not stat.S_ISREG(candidate.lstat().st_mode):
                self.error(f"evidence artifact is not a regular file: {relative}")
                return None
        except OSError:
            self.error(f"missing required artifact: {relative}")
            return None
        return candidate

    def load_json(self, relative: str) -> dict[str, Any]:
        path = self.evidence_file(relative)
        if path is None:
            return {}
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            self.error(f"missing required artifact: {relative}")
            return {}
        except (OSError, json.JSONDecodeError) as exc:
            self.error(f"invalid {relative}: {exc}")
            return {}
        if not isinstance(value, dict):
            self.error(f"{relative} must contain a JSON object")
            return {}
        return value

    def require_nonempty(self, value: Any, label: str) -> None:
        if not isinstance(value, str) or not value.strip():
            self.error(f"missing required provenance: {label}")

    def validate(self) -> None:
        manifest = self.load_json("manifest.json")
        if not manifest:
            return
        mode = manifest.get("mode")
        if mode not in {"qualification", "exploratory"}:
            self.error("manifest mode must be qualification or exploratory")
        elif mode != self.expected_mode:
            self.error(
                f"manifest mode={mode!r}, expected {self.expected_mode!r} "
                "from the runner invocation"
            )
        if manifest.get("schema_version") != 1:
            self.error("unsupported manifest schema_version")

        model = manifest.get("model") if isinstance(manifest.get("model"), dict) else {}
        backend = (
            manifest.get("backend") if isinstance(manifest.get("backend"), dict) else {}
        )
        matrix = manifest.get("matrix") if isinstance(manifest.get("matrix"), dict) else {}
        requested_model = model.get("requested_id")
        expected_digest = model.get("digest")
        expected_context = model.get("context_window")
        backend_name = backend.get("name")
        max_rounds = matrix.get("max_rounds")
        tasks_dir = matrix.get("tasks_dir")

        self.require_nonempty(requested_model, "manifest.model.requested_id")
        self.require_nonempty(backend_name, "manifest.backend.name")
        if not isinstance(max_rounds, int) or max_rounds <= 0:
            self.error("manifest.matrix.max_rounds must be a positive integer")

        dimensions: list[list[str]] = []
        for key in ("postures", "ocap_modes", "tasks"):
            values = matrix.get(key)
            if (
                not isinstance(values, list)
                or not values
                or any(not isinstance(v, str) or not v for v in values)
                or len(set(values)) != len(values)
            ):
                self.error(f"manifest.matrix.{key} must be a non-empty unique string list")
                dimensions.append([])
            else:
                dimensions.append(values)
        expected_cells = set(itertools.product(*dimensions)) if all(dimensions) else set()
        self.validate_retained_artifacts(manifest, expected_cells)
        if self.expected_mode == "qualification":
            postures = matrix.get("postures")
            ocaps = matrix.get("ocap_modes")
            if not isinstance(postures, list) or set(postures) != QUALIFICATION_POSTURES:
                self.error(
                    "qualification requires the complete posture set: "
                    "baseline, tenacity, crew, obsessive"
                )
            if not isinstance(ocaps, list) or set(ocaps) != QUALIFICATION_OCAPS:
                self.error("qualification requires the complete OCAP set: off, on")
        self.validate_rows(
            expected_cells,
            requested_model,
            expected_digest,
            expected_context,
            backend_name,
            max_rounds,
            tasks_dir,
        )

        if self.expected_mode == "qualification":
            self.validate_qualification_provenance(manifest)

    def validate_retained_artifacts(
        self,
        manifest: dict[str, Any],
        expected_cells: set[tuple[str, str, str]],
    ) -> None:
        server = manifest.get("server") if isinstance(manifest.get("server"), dict) else {}
        required = {
            "manifest.json",
            "results.csv",
            "matrix.md",
            "config.toml",
            "provenance.json",
            "harness-sources.json",
            "server-models.json",
        }
        if self.expected_mode == "qualification":
            required.update(
                {
                    "server-launch-manifest.json",
                    "preflight/config.toml",
                    "preflight/port",
                    "preflight/instruction.txt",
                    "preflight/first-request.json",
                    "preflight/second-request.json",
                    "preflight/events.jsonl",
                    "preflight/server.trace",
                    "preflight/newt.trace",
                }
            )
            if server.get("kind") == "vllm":
                required.add("server-version.json")
            elif server.get("kind") == "llama_cpp":
                required.add("server-props.json")
        for posture, ocap, task in expected_cells:
            cell = f"{posture}-{ocap}-{task}"
            required.update(
                {
                    f"events/{cell}.jsonl",
                    f"traces/{cell}.trace",
                    f"traces/{cell}.setup.trace",
                    f"workspace-baselines/{cell}.json",
                }
            )
        for relative in sorted(required):
            self.evidence_file(relative)

    def validate_rows(
        self,
        expected_cells: set[tuple[str, str, str]],
        requested_model: Any,
        expected_digest: Any,
        expected_context: Any,
        backend_name: Any,
        max_rounds: Any,
        tasks_dir: Any,
    ) -> None:
        path = self.evidence_file("results.csv")
        if path is None:
            return
        try:
            with path.open(newline="", encoding="utf-8") as handle:
                reader = csv.DictReader(handle)
                fields = set(reader.fieldnames or [])
                if not REQUIRED_CSV_FIELDS.issubset(fields):
                    missing = sorted(REQUIRED_CSV_FIELDS - fields)
                    self.error(f"results.csv missing columns: {', '.join(missing)}")
                    return
                rows = list(reader)
        except OSError as exc:
            self.error(f"cannot read results.csv: {exc}")
            return

        found: dict[tuple[str, str, str], dict[str, str]] = {}
        evidence_owners: dict[str, str] = {}
        for number, row in enumerate(rows, start=2):
            cell = (row["posture"], row["ocap"], row["task"])
            label = "/".join(cell)
            if cell in found:
                self.error(f"duplicate required cell: {label}")
                continue
            found[cell] = row
            if cell not in expected_cells:
                self.error(f"unexpected cell in results.csv line {number}: {label}")
            if row["verify"] != "pass":
                self.error(f"{label}: verification was {row['verify']!r}, not pass")
            try:
                solve_rc = int(row["solve_rc"])
            except ValueError:
                solve_rc = -1
            if solve_rc != 0:
                self.error(f"{label}: solve_rc was {row['solve_rc']!r}, not 0")
            if row["status"] != "completed":
                self.error(f"{label}: solve status was {row['status']!r}, not completed")
            expected_events = f"events/{row['posture']}-{row['ocap']}-{row['task']}.jsonl"
            if row["events_file"] != expected_events:
                self.error(
                    f"{label}: events_file={row['events_file']!r}, expected runner-owned "
                    f"path {expected_events!r}"
                )
            previous_owner = evidence_owners.get(row["events_file"])
            if previous_owner is not None:
                self.error(
                    f"{label}: events_file {row['events_file']!r} is reused by "
                    f"{previous_owner}"
                )
            else:
                evidence_owners[row["events_file"]] = label
            self.validate_events(
                label,
                row["events_file"],
                requested_model,
                expected_digest,
                expected_context,
                backend_name,
                max_rounds,
                row["ocap"],
                row["posture"],
                row["task"],
                tasks_dir,
            )
            self.validate_workspace_baseline(
                row["posture"], row["ocap"], row["task"], tasks_dir
            )

        for cell in sorted(expected_cells - set(found)):
            self.error(f"missing required cell: {'/'.join(cell)}")

    def validate_events(
        self,
        label: str,
        relative: str,
        requested_model: Any,
        expected_digest: Any,
        expected_context: Any,
        backend_name: Any,
        max_rounds: Any,
        ocap: str,
        posture: str,
        task: str,
        tasks_dir: Any,
    ) -> None:
        if not relative:
            self.error(f"{label}: events_file is empty")
            return
        path = self.evidence_file(relative)
        if path is None:
            return
        try:
            raw_lines = path.read_text(encoding="utf-8").splitlines()
        except OSError as exc:
            self.error(f"{label}: cannot read {relative}: {exc}")
            return

        records: list[dict[str, Any]] = []
        for line_number, raw in enumerate(raw_lines, start=1):
            if not raw.strip():
                continue
            try:
                value = json.loads(raw)
            except json.JSONDecodeError as exc:
                self.error(f"{label}: invalid JSONL line {line_number}: {exc}")
                continue
            if not isinstance(value, dict):
                self.error(f"{label}: JSONL line {line_number} is not an object")
                continue
            records.append(value)
        expected_task_file = (
            str((Path(tasks_dir) / task / "instruction.txt").resolve())
            if isinstance(tasks_dir, str)
            else None
        )
        self.validate_solve_result(
            label,
            records,
            expected_task_file,
            str((self.root / "ws" / f"{posture}-{ocap}-{task}").resolve()),
            requested_model,
        )
        contract = self.single_contract(label, records)
        if contract is None:
            return
        self.validate_contract(
            label,
            contract,
            requested_model,
            expected_digest,
            expected_context,
            backend_name,
            max_rounds,
            POSTURE_TENACITY.get(posture),
            POSTURE_COGNITION.get(posture),
            POSTURE_CREW.get(posture),
            ocap,
        )

    def validate_workspace_baseline(
        self, posture: str, ocap: str, task: str, tasks_dir: Any
    ) -> None:
        cell = f"{posture}-{ocap}-{task}"
        relative = f"workspace-baselines/{cell}.json"
        proof = self.load_json(relative)
        label = f"{posture}/{ocap}/{task}"
        if not proof:
            self.error(f"{label}: workspace baseline proof is missing or invalid")
            return
        expected_workspace = str((self.root / "ws" / cell).resolve())
        expected = {
            "schema_version": 1,
            "cell": cell,
            "posture": posture,
            "ocap": ocap,
            "task": task,
            "workspace": expected_workspace,
        }
        for key, value in expected.items():
            if proof.get(key) != value:
                self.error(
                    f"{label}: workspace baseline {key}={proof.get(key)!r}, "
                    f"expected {value!r}"
                )

        git = proof.get("git") if isinstance(proof.get("git"), dict) else {}
        if git.get("root") != expected_workspace:
            self.error(f"{label}: workspace baseline Git root is not canonical")
        for key in ("baseline_commit", "baseline_tree"):
            value = git.get(key)
            if not isinstance(value, str) or not OBJECT_ID_RE.fullmatch(value):
                self.error(f"{label}: workspace baseline {key} is not a full object ID")
        if git.get("clean") is not True:
            self.error(f"{label}: workspace baseline Git index/worktree was not clean")

        just = proof.get("just") if isinstance(proof.get("just"), dict) else {}
        if just.get("file") not in {"justfile", "Justfile"}:
            self.error(f"{label}: workspace baseline Justfile identity is invalid")
        for key in ("sha256", "dry_run_sha256"):
            value = just.get(key)
            if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
                detail = "dry-run" if key == "dry_run_sha256" else "Justfile"
                self.error(f"{label}: workspace baseline {detail} SHA-256 is invalid")
        git_blob = just.get("git_blob")
        if not isinstance(git_blob, str) or not OBJECT_ID_RE.fullmatch(git_blob):
            self.error(f"{label}: workspace baseline Justfile Git blob is invalid")
        recipes = just.get("recipes")
        if (
            not isinstance(recipes, list)
            or "check" not in recipes
            or any(not isinstance(recipe, str) or not recipe for recipe in recipes)
            or len(set(recipes)) != len(recipes)
        ):
            self.error(f"{label}: workspace baseline lacks a unique check recipe")
        if not isinstance(just.get("generated"), bool):
            self.error(f"{label}: workspace baseline Justfile origin is missing")

        verifier = (
            proof.get("verifier")
            if isinstance(proof.get("verifier"), dict)
            else {}
        )
        expected_verifier = (
            str((Path(tasks_dir) / task / "verify.sh").resolve())
            if isinstance(tasks_dir, str)
            else None
        )
        if verifier.get("path") != expected_verifier:
            self.error(f"{label}: workspace baseline verifier path is not canonical")
        verifier_hash = verifier.get("sha256")
        if not isinstance(verifier_hash, str) or not SHA256_RE.fullmatch(verifier_hash):
            self.error(f"{label}: workspace baseline verifier SHA-256 is invalid")
        else:
            self.baseline_verifier_hashes[(posture, ocap, task)] = verifier_hash

    def validate_solve_result(
        self,
        label: str,
        records: list[dict[str, Any]],
        expected_task_file: Any,
        expected_cwd: str,
        requested_model: Any,
    ) -> None:
        solve_results = [record for record in records if record.get("kind") == "solve_result"]
        if len(solve_results) != 1:
            self.error(
                f"{label}: expected exactly one solve_result record, "
                f"found {len(solve_results)}"
            )
            return
        solve_result = solve_results[0]
        expected = {
            "task_file": expected_task_file,
            "cwd": expected_cwd,
            "model": requested_model,
            "backend_kind": "openai",
            "status": "completed",
        }
        for key, value in expected.items():
            if solve_result.get(key) != value:
                self.error(
                    f"{label}: solve_result {key}={solve_result.get(key)!r}, "
                    f"expected {value!r}"
                )

    def single_contract(
        self, label: str, records: list[dict[str, Any]]
    ) -> dict[str, Any] | None:
        contracts = [record for record in records if "contract_version" in record]
        if len(contracts) != 1:
            self.error(
                f"{label}: expected exactly one contract record, found {len(contracts)}"
            )
            return None
        contract = contracts[0]
        if contract.get("contract_version") != "1":
            self.error(
                f"{label}: contract_version={contract.get('contract_version')!r}, expected '1'"
            )
            return None
        return contract

    def validate_contract(
        self,
        label: str,
        contract: dict[str, Any],
        requested_model: Any,
        expected_digest: Any,
        expected_context: Any,
        backend_name: Any,
        max_rounds: Any,
        tenacity: Any,
        cognition: Any,
        crew: Any,
        ocap: Any,
    ) -> None:
        comparisons = {
            "requested_model": requested_model,
            "effective_model": requested_model,
            "model_digest": expected_digest,
        }
        for key, expected in comparisons.items():
            if expected not in (None, "") and contract.get(key) != expected:
                self.error(
                    f"{label}: contract {key}={contract.get(key)!r}, expected {expected!r}"
                )
        if contract.get("outcome") != "completed":
            self.error(f"{label}: contract outcome is not completed")
        if contract.get("agent") != "newt-agent":
            self.error(f"{label}: contract agent must be 'newt-agent'")
        if not isinstance(contract.get("agent_version"), str) or not contract[
            "agent_version"
        ].strip():
            self.error(f"{label}: contract agent_version must be non-empty")
        backend = contract.get("backend")
        if not isinstance(backend, dict) or backend.get("name") != backend_name:
            self.error(f"{label}: contract backend name does not match the manifest")
        if not isinstance(backend, dict) or backend.get("kind") != "openai":
            self.error(f"{label}: contract backend kind must be openai")
        effective = contract.get("effective_config")
        if not isinstance(effective, dict):
            self.error(f"{label}: contract effective_config is missing")
            return
        if expected_context not in (None, 0, "") and effective.get("context_window") != expected_context:
            self.error(f"{label}: contract context_window does not match the manifest")
        if isinstance(max_rounds, int) and effective.get("max_rounds") != max_rounds:
            self.error(f"{label}: contract max_rounds does not match the manifest")
        if effective.get("ocap") != ocap:
            self.error(f"{label}: contract OCAP lane does not match expected evidence")
        if tenacity is not None and effective.get("tenacity") != tenacity:
            self.error(
                f"{label}: contract tenacity={effective.get('tenacity')!r}, "
                f"expected {tenacity!r}"
            )
        if cognition is not None and effective.get("cognition") != cognition:
            self.error(
                f"{label}: contract cognition={effective.get('cognition')!r}, "
                f"expected {cognition!r}"
            )
        if crew is not None and effective.get("crew") != crew:
            self.error(
                f"{label}: contract crew={effective.get('crew')!r}, "
                f"expected {crew!r}"
            )

    def validate_qualification_provenance(self, manifest: dict[str, Any]) -> None:
        model = manifest.get("model", {})
        backend = manifest.get("backend", {})
        declared_server = (
            manifest.get("server") if isinstance(manifest.get("server"), dict) else {}
        )
        matrix = manifest.get("matrix", {})
        if backend.get("api") != "chat_completions":
            self.error("qualification requires backend.api=chat_completions")
        if backend.get("capability_profile") != "nemotron":
            self.error("qualification requires capability_profile=nemotron")
        digest = model.get("digest")
        if not isinstance(digest, str) or not MODEL_DIGEST_RE.fullmatch(digest):
            self.error(
                "qualification model digest must be 40 hex, 64 hex, or sha256:<64 hex>"
            )
        if not isinstance(model.get("context_window"), int) or model["context_window"] <= 0:
            self.error("qualification requires a positive context window")
        tasks_dir = matrix.get("tasks_dir")
        if (
            not isinstance(tasks_dir, str)
            or not Path(tasks_dir).is_absolute()
            or str(Path(tasks_dir).resolve()) != tasks_dir
        ):
            self.error("manifest.matrix.tasks_dir must be a canonical absolute path")
        # The setup.sh-presence fact must be pinned so revalidation is hermetic
        # (see build_source_manifest_requirements). A subset of the declared tasks.
        tasks_with_setup = matrix.get("tasks_with_setup")
        tasks = matrix.get("tasks")
        if not isinstance(tasks_with_setup, list) or not all(
            isinstance(t, str) for t in tasks_with_setup
        ):
            self.error("manifest.matrix.tasks_with_setup must be a list of task names")
        elif isinstance(tasks, list) and not set(tasks_with_setup).issubset(set(tasks)):
            self.error("manifest.matrix.tasks_with_setup must be a subset of matrix.tasks")

        provenance = self.load_json("provenance.json")
        if provenance.get("schema_version") != 1:
            self.error("unsupported provenance schema_version")
        self.require_nonempty(provenance.get("captured_at_utc"), "captured_at_utc")
        observed_model = provenance.get("model", {})
        if observed_model != {
            "id": model.get("requested_id"),
            "digest": model.get("digest"),
            "context_window": model.get("context_window"),
        }:
            self.error("provenance model identity does not match the manifest")

        server = provenance.get("server") if isinstance(provenance.get("server"), dict) else {}
        for key in (
            "endpoint",
            "kind",
            "version",
            "chat_template_id",
            "tool_parser_id",
            "reasoning_parser_id",
        ):
            self.require_nonempty(server.get(key), f"server.{key}")
        kind = server.get("kind")
        if kind not in {"vllm", "llama_cpp"}:
            self.error("server.kind must be vllm or llama_cpp")
        if server.get("probe_ok") is not True:
            self.error("server provenance probe did not succeed")
        for field in (
            "endpoint",
            "kind",
            "version",
            "chat_template_id",
            "tool_parser_id",
            "reasoning_parser_id",
        ):
            if server.get(field) != declared_server.get(field):
                self.error(f"provenance server.{field} does not match manifest.json")

        launch = self.load_json("server-launch-manifest.json")
        launch_expected = {
            "model_id": model.get("requested_id"),
            "model_digest": model.get("digest"),
            "context_window": model.get("context_window"),
            "server_kind": declared_server.get("kind"),
            "server_version": declared_server.get("version"),
            "chat_template_id": declared_server.get("chat_template_id"),
            "tool_parser_id": declared_server.get("tool_parser_id"),
            "reasoning_parser_id": declared_server.get("reasoning_parser_id"),
        }
        self.errors.extend(launch_manifest_errors(launch, launch_expected))
        launch_path = self.evidence_file("server-launch-manifest.json")
        try:
            launch_digest = sha256_file(launch_path) if launch_path is not None else None
        except OSError as exc:
            self.error(f"cannot hash retained server launch manifest: {exc}")
            launch_digest = None
        if declared_server.get("launch_manifest_sha256") != launch_digest:
            self.error("manifest launch manifest SHA-256 does not match retained artifact")
        launch_provenance = server.get("launch_manifest")
        if not isinstance(launch_provenance, dict):
            self.error("provenance server launch manifest identity is missing")
        elif (
            launch_provenance.get("artifact") != "server-launch-manifest.json"
            or launch_provenance.get("sha256") != launch_digest
        ):
            self.error("provenance launch manifest SHA-256 does not match retained artifact")

        models_response = self.load_json("server-models.json")
        self.errors.extend(
            models_response_errors(models_response, model.get("requested_id"))
        )
        if kind == "vllm":
            if server.get("tool_parser_id") != "qwen3_coder":
                self.error("vLLM reference lane requires tool_parser_id=qwen3_coder")
            if server.get("reasoning_parser_id") != "nano_v3":
                self.error("vLLM reference lane requires reasoning_parser_id=nano_v3")
            version_response = self.load_json("server-version.json")
            self.errors.extend(
                server_probe_errors(
                    kind,
                    server.get("version"),
                    model.get("context_window"),
                    version_response=version_response,
                )
            )
        elif kind == "llama_cpp":
            props_response = self.load_json("server-props.json")
            self.errors.extend(
                server_probe_errors(
                    kind,
                    server.get("version"),
                    model.get("context_window"),
                    props_response=props_response,
                    chat_template_id=server.get("chat_template_id"),
                )
            )

        newt = provenance.get("newt") if isinstance(provenance.get("newt"), dict) else {}
        for key in ("path", "version"):
            self.require_nonempty(newt.get(key), f"newt.{key}")
        before = newt.get("sha256_before")
        after = newt.get("sha256_after")
        if not isinstance(before, str) or not SHA256_RE.fullmatch(before):
            self.error("newt.sha256_before is not a SHA-256 digest")
        if not isinstance(after, str) or not SHA256_RE.fullmatch(after):
            self.error("newt.sha256_after is not a SHA-256 digest")
        if before != after:
            self.error("Newt binary changed while the matrix was running")
        harness = (
            provenance.get("harness")
            if isinstance(provenance.get("harness"), dict)
            else {}
        )
        if not isinstance(harness.get("commit"), str) or not COMMIT_RE.fullmatch(
            harness["commit"]
        ):
            self.error("harness.commit is not a full Git commit identity")
        if not isinstance(harness.get("dirty"), bool):
            self.error("harness.dirty must record true or false")
        self.validate_harness_sources(harness, matrix)

        self.validate_config(manifest)
        self.validate_preflight(manifest)

    def validate_harness_sources(
        self, harness: dict[str, Any], matrix: dict[str, Any]
    ) -> None:
        if harness.get("source_manifest") != "harness-sources.json":
            self.error("harness source manifest identity is missing")
        sources = self.load_json("harness-sources.json")
        if sources.get("schema_version") != 1:
            self.error("unsupported harness source manifest schema_version")
        files = sources.get("files")
        if not isinstance(files, list) or not files:
            self.error("harness source manifest must list source files")
            return
        before_entries: list[tuple[str, str]] = []
        after_entries: list[tuple[str, str]] = []
        source_hashes: dict[str, str] = {}
        labels: set[str] = set()
        for entry in files:
            if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
                self.error("invalid harness source file entry")
                continue
            label = entry["path"]
            if label in labels:
                self.error(f"duplicate harness source path: {label}")
            labels.add(label)
            before = entry.get("sha256_before")
            after = entry.get("sha256_after")
            if not isinstance(before, str) or not SHA256_RE.fullmatch(before):
                self.error(f"invalid before hash for harness source {label}")
            else:
                before_entries.append((label, before))
                source_hashes[label] = before
            if not isinstance(after, str) or not SHA256_RE.fullmatch(after):
                self.error(f"invalid after hash for harness source {label}")
            else:
                after_entries.append((label, after))
            if before != after:
                self.error(f"harness source changed while running: {label}")
        before_aggregate = aggregate_source_entries(before_entries)
        after_aggregate = aggregate_source_entries(after_entries)
        if sources.get("aggregate_sha256_before") != before_aggregate:
            self.error("harness source before aggregate is invalid")
        if sources.get("aggregate_sha256_after") != after_aggregate:
            self.error("harness source after aggregate is invalid")
        if sources.get("differences") != []:
            self.error("harness source differences were recorded")
        if harness.get("source_sha256_before") != before_aggregate:
            self.error("provenance harness source before hash does not match")
        if harness.get("source_sha256_after") != after_aggregate:
            self.error("provenance harness source after hash does not match")

        required_labels = {f"harness/{name}" for name in HARNESS_FILES}
        tasks = matrix.get("tasks")
        # Hermetic: which tasks carried a setup.sh is a fact PINNED in the manifest
        # at run time. Revalidation reads it and NEVER stat()s the live task tree,
        # so a bundle validates identically after the checkout moves or a task
        # gains/loses a setup.sh later.
        tasks_with_setup = matrix.get("tasks_with_setup")
        if isinstance(tasks, list):
            for task in tasks:
                if not isinstance(task, str):
                    continue
                required_labels.update(
                    {
                        f"tasks/{task}/instruction.txt",
                        f"tasks/{task}/verify.sh",
                    }
                )
                if isinstance(tasks_with_setup, list) and task in tasks_with_setup:
                    required_labels.add(f"tasks/{task}/setup.sh")
        for missing in sorted(required_labels - labels):
            self.error(f"harness source manifest lacks required source {missing}")
        for (posture, ocap, task), observed in self.baseline_verifier_hashes.items():
            expected = source_hashes.get(f"tasks/{task}/verify.sh")
            if observed != expected:
                self.error(
                    f"{posture}/{ocap}/{task}: workspace baseline verifier SHA-256 "
                    "does not match the pinned task source"
                )

    def require_file(self, relative: str) -> None:
        path = self.evidence_file(relative)
        if path is None:
            return
        try:
            if path.stat().st_size == 0:
                self.error(f"missing required artifact: {relative}")
        except OSError:
            self.error(f"missing required artifact: {relative}")

    def load_jsonl(self, relative: str, label: str) -> list[dict[str, Any]]:
        path = self.evidence_file(relative)
        if path is None:
            return []
        try:
            raw_lines = path.read_text(encoding="utf-8").splitlines()
        except OSError as exc:
            self.error(f"{label}: cannot read {relative}: {exc}")
            return []
        records: list[dict[str, Any]] = []
        for line_number, raw in enumerate(raw_lines, start=1):
            if not raw.strip():
                continue
            try:
                value = json.loads(raw)
            except json.JSONDecodeError as exc:
                self.error(f"{label}: invalid JSONL line {line_number}: {exc}")
                continue
            if not isinstance(value, dict):
                self.error(f"{label}: JSONL line {line_number} is not an object")
                continue
            records.append(value)
        return records

    def validate_config(self, manifest: dict[str, Any]) -> None:
        backend = manifest.get("backend") if isinstance(manifest.get("backend"), dict) else {}
        model = manifest.get("model") if isinstance(manifest.get("model"), dict) else {}
        server = manifest.get("server") if isinstance(manifest.get("server"), dict) else {}
        path = self.evidence_file("config.toml")
        if path is None:
            return
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            self.error(f"cannot read config.toml: {exc}")
            return
        errors = generated_config_errors(
            text,
            backend.get("name"),
            server.get("endpoint"),
            model.get("requested_id"),
            api="chat_completions",
        )
        for error in errors:
            self.error(f"config.toml: {error}")

    def validate_preflight(self, manifest: dict[str, Any]) -> None:
        backend = manifest.get("backend") if isinstance(manifest.get("backend"), dict) else {}
        model = manifest.get("model") if isinstance(manifest.get("model"), dict) else {}
        requested_model = model.get("requested_id")
        port_path = self.evidence_file("preflight/port")
        if port_path is None:
            port_text = ""
        else:
            try:
                port_text = port_path.read_text(encoding="utf-8")
            except OSError as exc:
                self.error(f"cannot read preflight/port: {exc}")
                port_text = ""
        if not re.fullmatch(r"[0-9]{1,5}\n", port_text):
            self.error("preflight/port must contain one generated TCP port")
            preflight_endpoint = ""
        else:
            port = int(port_text)
            if not 1 <= port <= 65535:
                self.error("preflight/port is outside the TCP port range")
            preflight_endpoint = f"http://127.0.0.1:{port}"

        config_path = self.evidence_file("preflight/config.toml")
        if config_path is None:
            config_text = ""
        else:
            try:
                config_text = config_path.read_text(encoding="utf-8")
            except OSError as exc:
                self.error(f"cannot read preflight/config.toml: {exc}")
                config_text = ""
        for error in generated_config_errors(
            config_text,
            backend.get("name"),
            preflight_endpoint,
            requested_model,
            api="chat_completions",
            allow_api_key_file=False,
        ):
            self.error(f"preflight/config.toml: {error}")

        request = self.load_json("preflight/first-request.json")
        self.errors.extend(first_request_errors(request, requested_model))
        self.require_file("preflight/second-request.json")
        second_request = self.load_json("preflight/second-request.json")
        self.errors.extend(
            second_request_errors(second_request, requested_model, request)
        )

        records = self.load_jsonl("preflight/events.jsonl", "preflight")
        self.validate_solve_result(
            "preflight",
            records,
            str((self.root / "preflight" / "instruction.txt").resolve()),
            str((self.root / "preflight" / "ws").resolve()),
            requested_model,
        )
        contract = self.single_contract("preflight", records)
        if contract is not None:
            self.validate_contract(
                "preflight",
                contract,
                requested_model,
                model.get("digest"),
                model.get("context_window"),
                backend.get("name"),
                2,
                "standard",
                "contemplating",
                "off",
                "off",
            )

        overflow = [record for record in records if record.get("kind") == "reasoning_overflow"]
        if len(overflow) != 1:
            self.error(
                "preflight: expected exactly one reasoning_overflow record, "
                f"found {len(overflow)}"
            )
        else:
            signal = overflow[0]
            for key in (
                "reasoning_overflow_detected",
                "continuation_attempted",
                "continuation_succeeded",
            ):
                if signal.get(key) is not True:
                    self.error(f"preflight: reasoning_overflow {key} must be true")
            if signal.get("finish_reason") != "length":
                self.error(
                    "preflight: reasoning_overflow finish_reason must be 'length'"
                )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--expected-mode",
        required=True,
        choices=("qualification", "exploratory"),
        help="mode selected by the runner before any model-controlled work",
    )
    parser.add_argument("run_dir", type=Path)
    args = parser.parse_args()
    validator = RunValidator(args.run_dir, args.expected_mode)
    validator.validate()
    if validator.errors:
        print(f"{validator.mode} validation failed ({len(validator.errors)} error(s)):")
        for error in validator.errors:
            print(f"  - {error}")
        return 1
    print(f"{validator.mode} validation passed: every required cell and artifact is valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())
