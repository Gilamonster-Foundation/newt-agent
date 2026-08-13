#!/usr/bin/env python3
"""Shared, dependency-free evidence checks for the qualification harness."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import shutil
import sys
from pathlib import Path
from typing import Any, Iterable


LAUNCH_FIELDS = (
    "model_id",
    "model_digest",
    "context_window",
    "server_kind",
    "server_version",
    "chat_template_id",
    "tool_parser_id",
    "reasoning_parser_id",
)
HARNESS_FILES = (
    ".gitignore",
    "README.md",
    "loopback-preflight.py",
    "qualification_harness.py",
    "run-matrix.sh",
    "validate-run.py",
)
REASONING_REPLAY_MARKER = "NEWT_QUALIFICATION_REASONING_REPLAY_MARKER_v1"
PREFLIGHT_PROMPT = (
    "Answer briefly that the qualification preflight is complete. Do not call a tool."
)
# Newt's prompt-provenance system card (newt-core prompt_read.rs): the protected
# active-prompt pair is a metadata system card with this prefix followed by the
# exact operator text, kept as a compression-proof recovery copy in addition to
# the live tail user turn.
ACTIVE_PROMPT_CARD_PREFIX = "[NEWT ACTIVE PROMPT"
TOOL_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_-]{0,63}$")


def generated_config_errors(
    text: str,
    backend_name: Any,
    endpoint: Any,
    model: Any,
    *,
    api: str = "chat_completions",
    allow_api_key_file: bool = True,
) -> list[str]:
    """Validate the exact ordered TOML emitted by ``render_config``.

    Python 3.9 has no stdlib TOML parser. This validator intentionally accepts
    the much smaller language the runner itself emits, including one optional
    JSON-quoted ``api_key_file`` declaration in its generated slot.
    """
    errors: list[str] = []
    if not all(isinstance(value, str) and value for value in (backend_name, endpoint, model)):
        return ["generated config expectations must be non-empty strings"]
    if not text.endswith("\n") or "\r" in text:
        errors.append("config must use generated LF-terminated lines")

    lines = text.splitlines()
    api_key_index = 7
    if len(lines) > api_key_index and lines[api_key_index].startswith("api_key_file = "):
        declaration = lines.pop(api_key_index)
        encoded = declaration.removeprefix("api_key_file = ")
        if not allow_api_key_file:
            errors.append("generated config must not contain api_key_file")
        try:
            api_key_file = json.loads(encoded)
        except json.JSONDecodeError:
            api_key_file = None
        if (
            not isinstance(api_key_file, str)
            or not api_key_file
            or json.dumps(api_key_file) != encoded
        ):
            errors.append("api_key_file must be a non-empty generated JSON string")

    expected = [
        f"default_backend = {json.dumps(backend_name)}",
        "[[backends]]",
        f"name = {json.dumps(backend_name)}",
        f"endpoint = {json.dumps(endpoint)}",
        f"model = {json.dumps(model)}",
        'kind = "openai"',
        f"api = {json.dumps(api)}",
        'tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]',
        "[backends.capability]",
        'reasoning_replay_scope = "current_user_turn"',
        "[backends.capability.chat_completions]",
        "cognition = true",
        "chat_template_kwargs = true",
        "parallel_tool_calls = false",
        "bounded_reasoning_continuation = true",
    ]
    if lines != expected:
        errors.append("config does not match the exact generated Nemotron shape and values")
    return errors


def _request_policy_errors(
    request: dict[str, Any], requested_model: Any
) -> list[str]:
    errors: list[str] = []
    checks = {
        "model": requested_model,
        "max_tokens": 16000,
        "parallel_tool_calls": False,
    }
    for key, expected in checks.items():
        if key not in request or request.get(key) != expected:
            errors.append(f"preflight {key}={request.get(key)!r}, expected {expected!r}")
    for key, expected in (("temperature", 0.6), ("top_p", 0.95)):
        value = request.get(key)
        if not isinstance(value, (int, float)) or not math.isclose(value, expected):
            errors.append(f"preflight {key}={value!r}, expected {expected}")
    kwargs = request.get("chat_template_kwargs")
    if not isinstance(kwargs, dict):
        errors.append("preflight chat_template_kwargs is missing")
    else:
        for key in ("enable_thinking", "truncate_history_thinking"):
            if kwargs.get(key) is not True:
                errors.append(f"preflight chat_template_kwargs.{key} must be true")
    tools = request.get("tools")
    if not isinstance(tools, list) or not tools:
        errors.append("preflight request must contain Newt's tools")
    else:
        names: list[str] = []
        for index, tool in enumerate(tools):
            function = tool.get("function") if isinstance(tool, dict) else None
            name = function.get("name") if isinstance(function, dict) else None
            parameters = (
                function.get("parameters") if isinstance(function, dict) else None
            )
            if (
                not isinstance(tool, dict)
                or tool.get("type") != "function"
                or not isinstance(function, dict)
                or not isinstance(name, str)
                or not TOOL_NAME_RE.fullmatch(name)
                or not isinstance(parameters, dict)
            ):
                errors.append(
                    f"preflight tools[{index}] must be a valid named function tool"
                )
            else:
                names.append(name)
        if len(names) != len(set(names)):
            errors.append("preflight function tool names must be unique")
    if request.get("tool_choice") != "auto":
        errors.append("preflight tool_choice must be auto")
    return errors


def _contains_replay_marker(value: Any) -> bool:
    if isinstance(value, str):
        return REASONING_REPLAY_MARKER in value
    if isinstance(value, list):
        return any(_contains_replay_marker(item) for item in value)
    if isinstance(value, dict):
        return any(_contains_replay_marker(item) for item in value.values())
    return False


def first_request_errors(request: dict[str, Any], requested_model: Any) -> list[str]:
    """Return every initial Nemotron request-policy mismatch."""
    errors = _request_policy_errors(request, requested_model)
    messages = request.get("messages")
    prompt_indexes = (
        [
            index
            for index, message in enumerate(messages)
            if isinstance(message, dict)
            and message.get("role") == "user"
            and message.get("content") == PREFLIGHT_PROMPT
        ]
        if isinstance(messages, list)
        else []
    )

    # Newt sends the operator prompt as the live TAIL user turn. Since the
    # prompt-provenance card landed on main (prompt_read.rs: the protected
    # active-prompt pair), the identical text legitimately appears at most ONE
    # extra time — the compression-proof recovery copy immediately after the
    # "[NEWT ACTIVE PROMPT …]" system card. Anything else — no tail copy, a
    # duplicate without its card, or more than two copies — is a policy break.
    def is_recovery_copy(index: int) -> bool:
        if index == 0:
            return False
        previous = messages[index - 1]
        return (
            isinstance(previous, dict)
            and previous.get("role") == "system"
            and str(previous.get("content", "")).startswith(ACTIVE_PROMPT_CARD_PREFIX)
        )

    tail_is_prompt = bool(prompt_indexes) and prompt_indexes[-1] == len(messages) - 1
    shape_ok = tail_is_prompt and (
        len(prompt_indexes) == 1
        or (len(prompt_indexes) == 2 and is_recovery_copy(prompt_indexes[0]))
    )
    if not shape_ok:
        errors.append(
            "first request must carry the qualification user prompt as the live tail "
            "turn, plus at most one card-adjacent recovery copy"
        )
    if _contains_replay_marker(messages):
        errors.append("first request must not contain the reasoning replay marker")
    return errors


def second_request_errors(
    request: dict[str, Any], requested_model: Any, first_request: dict[str, Any]
) -> list[str]:
    """Validate continuation policy plus exact current-turn reasoning replay."""
    errors = _request_policy_errors(request, requested_model)
    first_messages = first_request.get("messages")
    messages = request.get("messages")
    appended = (
        messages[-1]
        if isinstance(messages, list) and len(messages) > 0
        else None
    )
    prefix_preserved = (
        isinstance(first_messages, list)
        and isinstance(messages, list)
        and len(messages) == len(first_messages) + 1
        and messages[:-1] == first_messages
    )
    if not prefix_preserved:
        errors.append(
            "second request must preserve first-request messages and append one message"
        )
    if (
        not isinstance(appended, dict)
        or appended.get("role") != "assistant"
        or appended.get("reasoning_content") != REASONING_REPLAY_MARKER
        or appended.get("content") is not None
    ):
        errors.append(
            "second request must append exactly one assistant reasoning replay marker"
        )
    marker_count = json.dumps(messages, sort_keys=True).count(REASONING_REPLAY_MARKER)
    if marker_count != 1:
        errors.append("second request must contain exactly one reasoning replay marker")
    return errors


def launch_manifest_errors(
    launch: dict[str, Any], expected: dict[str, Any]
) -> list[str]:
    errors: list[str] = []
    if launch.get("schema_version") != 1:
        errors.append("server launch manifest schema_version must be 1")
    for field in LAUNCH_FIELDS:
        if launch.get(field) != expected.get(field):
            errors.append(
                f"server launch manifest {field}={launch.get(field)!r}, "
                f"expected {expected.get(field)!r}"
            )
    return errors


def models_response_errors(response: dict[str, Any], model_id: Any) -> list[str]:
    data = response.get("data")
    if not isinstance(data, list):
        return ["server-models.json data must be a list"]
    served = {
        item.get("id")
        for item in data
        if isinstance(item, dict) and isinstance(item.get("id"), str)
    }
    if model_id not in served:
        return [f"server-models.json does not serve requested model {model_id!r}"]
    return []


def server_probe_errors(
    kind: Any,
    declared_version: Any,
    context_window: Any,
    version_response: Any = None,
    props_response: Any = None,
    chat_template_id: Any = None,
) -> list[str]:
    errors: list[str] = []
    if kind == "vllm":
        if not isinstance(version_response, dict):
            errors.append("server-version.json must contain an object")
        elif version_response.get("version") != declared_version:
            errors.append(
                f"vLLM version probe reported {version_response.get('version')!r}, "
                f"expected {declared_version!r}"
            )
    elif kind == "llama_cpp":
        if not isinstance(props_response, dict):
            errors.append("server-props.json must contain an object")
        else:
            if props_response.get("build_info") != declared_version:
                errors.append(
                    f"llama.cpp build_info={props_response.get('build_info')!r}, "
                    f"expected {declared_version!r}"
                )
            settings = props_response.get("default_generation_settings")
            if not isinstance(settings, dict):
                errors.append("llama.cpp default_generation_settings must be an object")
            elif settings.get("n_ctx") != context_window:
                errors.append(
                    f"llama.cpp n_ctx={settings.get('n_ctx')!r}, expected {context_window!r}"
                )
            template = props_response.get("chat_template")
            if not isinstance(template, str) or not template:
                errors.append("llama.cpp chat_template must be a non-empty string")
            else:
                observed_template_id = "sha256:" + hashlib.sha256(
                    template.encode("utf-8")
                ).hexdigest()
                if observed_template_id != chat_template_id:
                    errors.append(
                        f"llama.cpp chat template identity={observed_template_id!r}, "
                        f"expected {chat_template_id!r}"
                    )
    else:
        errors.append(f"unsupported inference server kind {kind!r}")
    return errors


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def aggregate_source_entries(entries: Iterable[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for label, file_digest in sorted(entries):
        digest.update(label.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_digest.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def discover_sources(harness_root: Path, tasks_root: Path) -> dict[str, Path]:
    sources: dict[str, Path] = {}
    for relative in HARNESS_FILES:
        path = harness_root / relative
        if path.is_file():
            sources[f"harness/{relative}"] = path
    tests = harness_root / "tests"
    if tests.is_dir():
        for path in sorted(tests.glob("test_*.py")):
            if path.is_file():
                sources[f"harness/tests/{path.name}"] = path
    for path in sorted(tasks_root.rglob("*")):
        if path.is_file():
            sources[f"tasks/{path.relative_to(tasks_root).as_posix()}"] = path
    return sources


def write_json_atomic(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def snapshot_sources(args: argparse.Namespace) -> int:
    harness_root = args.harness_root.resolve()
    tasks_root = args.tasks_root.resolve()
    sources = discover_sources(harness_root, tasks_root)
    if args.phase == "before":
        missing = [name for name in HARNESS_FILES if f"harness/{name}" not in sources]
        if missing:
            print(f"missing harness source files: {', '.join(missing)}", file=sys.stderr)
            return 1
        entries = [(label, sha256_file(path)) for label, path in sources.items()]
        aggregate = aggregate_source_entries(entries)
        value = {
            "schema_version": 1,
            "files": [
                {"path": label, "sha256_before": digest, "sha256_after": None}
                for label, digest in sorted(entries)
            ],
            "aggregate_sha256_before": aggregate,
            "aggregate_sha256_after": None,
            "differences": [],
        }
        write_json_atomic(args.output, value)
        print(aggregate)
        return 0

    try:
        value = json.loads(args.output.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"cannot read source snapshot: {exc}", file=sys.stderr)
        return 1
    previous = {
        entry.get("path"): entry
        for entry in value.get("files", [])
        if isinstance(entry, dict) and isinstance(entry.get("path"), str)
    }
    current = {label: sha256_file(path) for label, path in sources.items()}
    differences: list[str] = []
    for label, entry in previous.items():
        after = current.get(label)
        entry["sha256_after"] = after
        if after is None:
            differences.append(f"removed:{label}")
        elif entry.get("sha256_before") != after:
            differences.append(f"changed:{label}")
    for label, digest in sorted(current.items()):
        if label not in previous:
            value["files"].append(
                {"path": label, "sha256_before": None, "sha256_after": digest}
            )
            differences.append(f"added:{label}")
    aggregate = aggregate_source_entries(current.items())
    value["aggregate_sha256_after"] = aggregate
    value["differences"] = differences
    value["files"].sort(key=lambda entry: str(entry.get("path")))
    write_json_atomic(args.output, value)
    print(aggregate)
    return 1 if differences else 0


def load_object(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"invalid {label}: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"{label} must contain a JSON object")
    return value


def copy_launch_manifest(args: argparse.Namespace) -> int:
    try:
        launch = load_object(args.source, "server launch manifest")
    except ValueError as exc:
        print(exc, file=sys.stderr)
        return 1
    expected = {
        "model_id": args.model_id,
        "model_digest": args.model_digest,
        "context_window": args.context_window,
        "server_kind": args.server_kind,
        "server_version": args.server_version,
        "chat_template_id": args.chat_template_id,
        "tool_parser_id": args.tool_parser_id,
        "reasoning_parser_id": args.reasoning_parser_id,
    }
    errors = launch_manifest_errors(launch, expected)
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    args.destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(args.source, args.destination)
    print(sha256_file(args.destination))
    return 0


def validate_probes(args: argparse.Namespace) -> int:
    try:
        models = load_object(args.models, "server-models.json")
        errors = models_response_errors(models, args.model_id)
        if args.server_kind == "vllm":
            if args.version_file is None:
                raise ValueError("vLLM probe validation requires --version-file")
            version = load_object(args.version_file, "server-version.json")
            errors.extend(
                server_probe_errors(
                    args.server_kind, args.server_version, args.context_window, version
                )
            )
        else:
            if args.props_file is None:
                raise ValueError("llama.cpp probe validation requires --props-file")
            props = load_object(args.props_file, "server-props.json")
            errors.extend(
                server_probe_errors(
                    args.server_kind,
                    args.server_version,
                    args.context_window,
                    props_response=props,
                    chat_template_id=args.chat_template_id,
                )
            )
    except ValueError as exc:
        errors = [str(exc)]
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    copy_parser = subparsers.add_parser("copy-launch-manifest")
    copy_parser.add_argument("--source", required=True, type=Path)
    copy_parser.add_argument("--destination", required=True, type=Path)
    for option in (
        "model-id",
        "model-digest",
        "server-kind",
        "server-version",
        "chat-template-id",
        "tool-parser-id",
        "reasoning-parser-id",
    ):
        copy_parser.add_argument(f"--{option}", required=True)
    copy_parser.add_argument("--context-window", required=True, type=int)
    copy_parser.set_defaults(function=copy_launch_manifest)

    probe_parser = subparsers.add_parser("validate-probes")
    probe_parser.add_argument("--models", required=True, type=Path)
    probe_parser.add_argument("--model-id", required=True)
    probe_parser.add_argument("--server-kind", required=True)
    probe_parser.add_argument("--server-version", required=True)
    probe_parser.add_argument("--context-window", required=True, type=int)
    probe_parser.add_argument("--chat-template-id")
    probe_parser.add_argument("--version-file", type=Path)
    probe_parser.add_argument("--props-file", type=Path)
    probe_parser.set_defaults(function=validate_probes)

    source_parser = subparsers.add_parser("snapshot-sources")
    source_parser.add_argument("--phase", required=True, choices=("before", "after"))
    source_parser.add_argument("--output", required=True, type=Path)
    source_parser.add_argument("--harness-root", required=True, type=Path)
    source_parser.add_argument("--tasks-root", required=True, type=Path)
    source_parser.set_defaults(function=snapshot_sources)

    args = parser.parse_args()
    return args.function(args)


if __name__ == "__main__":
    sys.exit(main())
