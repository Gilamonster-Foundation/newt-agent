#!/usr/bin/env python3
"""Ephemeral stack build for local agent-bridle / agent-mesh branch testing."""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


def run(cmd: list[str], cwd: Path, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess:
    """Run a shell command with consistent error handling."""
    result = subprocess.run(
        cmd,
        cwd=str(cwd),
        check=False,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )
    if check and result.returncode != 0:
        if capture:
            raise RuntimeError(f"command failed: {' '.join(cmd)}\n{result.stdout}")
        raise RuntimeError(f"command failed: {' '.join(cmd)}")
    return result


def run_output(cmd: list[str], cwd: Path) -> str:
    return run(cmd, cwd=cwd, capture=True, check=True).stdout.strip()


@dataclass(frozen=True)
class RepoState:
    path: Path
    branch: str | None
    commit: str


def current_repo_state(path: Path) -> RepoState:
    if not path.is_dir():
        raise RuntimeError(f"repository missing: {path}")
    branch = run(["git", "symbolic-ref", "--short", "HEAD"], cwd=path, capture=True, check=False).stdout.strip()
    if not branch:
        branch = None
    commit = run_output(["git", "rev-parse", "HEAD"], cwd=path)
    return RepoState(path=path, branch=branch or None, commit=commit)


def restore_repo(state: RepoState) -> None:
    if state.branch:
        run(["git", "switch", "--quiet", state.branch], cwd=state.path)
    else:
        run(["git", "switch", "--quiet", "--detach", state.commit], cwd=state.path)


def ensure_repo_repo(path: Path, remote_name: str, branch: str) -> None:
    if not path.exists():
        raise RuntimeError(f"expected repository at {path}")
    if not (path / ".git").exists():
        raise RuntimeError(f"{path} is not a git checkout")
    if run(["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"], cwd=path, check=False).returncode != 0:
        run(["git", "fetch", "--quiet", remote_name, branch], cwd=path, check=True)
        if run(["git", "show-ref", "--verify", "--quiet", f"refs/remotes/{remote_name}/{branch}"], cwd=path, check=False).returncode != 0:
            raise RuntimeError(f"branch {branch} not found in {path} (nor origin/{branch})")
        run(
            [
                "git",
                "switch",
                "--quiet",
                "-c",
                branch,
                f"{remote_name}/{branch}",
            ],
            cwd=path,
        )
    else:
        run(["git", "switch", "--quiet", branch], cwd=path)


def replace_once(path: Path, pattern: str, dependency: str) -> None:
    original = path.read_text(encoding="utf-8")
    updated, count = re.subn(pattern, r'\g<1>"*"', original, count=1, flags=re.MULTILINE)
    if count != 1:
        raise RuntimeError(
            f"build-agent-stack: expected one {dependency} version in {path}, found {count}"
        )
    path.write_text(updated, encoding="utf-8")


def relax_dependency_versioning(root: Path) -> None:
    replace_once(
        root / "newt-core/Cargo.toml",
        r'^(\s*agent-bridle\s*=\s*\{[^\n]*version\s*=\s*)"[^"]+"',
        "agent-bridle (newt-core)",
    )
    replace_once(
        root / "newt-mcp-client/Cargo.toml",
        r'^(\s*agent-bridle\s*=\s*\{[^\n]*version\s*=\s*)"[^"]+"',
        "agent-bridle (newt-mcp-client)",
    )
    replace_once(
        root / "newt-mcp-server/Cargo.toml",
        r'^(\s*agent-bridle\s*=\s*\{[^\n]*version\s*=\s*)"[^"]+"',
        "agent-bridle (newt-mcp-server)",
    )
    replace_once(
        root / "Cargo.toml",
        r'^(\s*agent-mesh-protocol\s*=\s*)"[^"]+"',
        "agent-mesh-protocol",
    )


def add_stack_patch(root: Path) -> None:
    cargo = root / "Cargo.toml"
    existing = cargo.read_text(encoding="utf-8")
    patch_block = """\n# build-agent-stack temporary patch\n[patch.crates-io]\nagent-bridle = { path = \"../agent-bridle/agent-bridle\" }\nagent-mesh-protocol = { path = \"../agent-mesh/agent-mesh-protocol\" }\n"""
    cargo.write_text(existing + patch_block, encoding="utf-8")


def backup_and_edit(root: Path, backup_root: Path) -> None:
    backup_root.mkdir(parents=True, exist_ok=True)
    for rel in [
        Path("Cargo.toml"),
        Path("newt-core/Cargo.toml"),
        Path("newt-mcp-client/Cargo.toml"),
        Path("newt-mcp-server/Cargo.toml"),
        Path("Cargo.lock"),
    ]:
        (backup_root / rel).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / rel, backup_root / rel)

    relax_dependency_versioning(root)
    add_stack_patch(root)


def restore(root: Path, backup_root: Path) -> None:
    for rel in [
        Path("Cargo.toml"),
        Path("Cargo.lock"),
        Path("newt-core/Cargo.toml"),
        Path("newt-mcp-client/Cargo.toml"),
        Path("newt-mcp-server/Cargo.toml"),
    ]:
        src = backup_root / rel
        dst = root / rel
        if src.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("workspace", nargs="?", default=".")
    parser.add_argument("--agent-bridle-branch", default="main")
    parser.add_argument("--agent-mesh-branch", default="main")
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()
    if not workspace.is_dir():
        raise RuntimeError(f"workspace not found: {workspace}")

    bridle_repo = workspace.parent / "agent-bridle"
    mesh_repo = workspace.parent / "agent-mesh"

    bridle_state = current_repo_state(bridle_repo)
    mesh_state = current_repo_state(mesh_repo)

    with tempfile.TemporaryDirectory(prefix="build-agent-stack-") as tmpdir:
        tmp = Path(tmpdir)
        try:
            for rel in [
                Path("Cargo.toml"),
                Path("Cargo.lock"),
                Path("newt-core/Cargo.toml"),
                Path("newt-mcp-client/Cargo.toml"),
                Path("newt-mcp-server/Cargo.toml"),
            ]:
                (tmp / rel).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(workspace / rel, tmp / rel)

            ensure_repo_repo(bridle_repo, "origin", args.agent_bridle_branch)
            ensure_repo_repo(mesh_repo, "origin", args.agent_mesh_branch)

            backup_and_edit(workspace, tmp)

            run(["cargo", "build", "--workspace"], cwd=workspace)
            print("build-agent-stack: build complete")
        finally:
            restore(workspace, tmp)
            restore_repo(bridle_state)
            restore_repo(mesh_state)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("build-agent-stack: interrupted", file=sys.stderr)
        raise SystemExit(1)
    except Exception as exc:  # noqa: BLE001
        print(f"build-agent-stack: {exc}", file=sys.stderr)
        raise SystemExit(1)
