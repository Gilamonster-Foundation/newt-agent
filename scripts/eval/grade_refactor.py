#!/usr/bin/env python3
"""Grade a refactor run from Git/gh evidence, not the agent's assertions (#2804).

JSON stdout, short human report stderr, exit 0 PASS / 1 FAIL / 2 invalid input.
Only the disposable clone is mutated. GitHub access is read-only.
"""

from __future__ import annotations

import argparse
from functools import cache
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tempfile

from refactor_transcript import read_tail, positive_mib
from refactor_claims import check_claims, final_summary, operator_inputs

# Reuse the eval scoreboard's crate-vector-pinned content addressing, not a
# second encoder or ad-hoc digest. See newt-interaction/tests/vectors.rs.
sys.path.insert(
    0, str(Path(__file__).resolve().parents[2] / "newt-interaction/conformance")
)
from newt_conformance import content_id, encode  # noqa: E402


class EvidenceError(RuntimeError):
    """Unavailable evidence is a failing grade, not proof of fabrication."""


def run(argv: list[str], cwd: Path) -> str:
    """Execute a bounded argv (never a shell) in the selected checkout."""
    env = dict(os.environ)
    for key in ("CARGO_TARGET_DIR", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    env.update(RUSTC_WRAPPER="", CARGO_BUILD_JOBS="4", GIT_TERMINAL_PROMPT="0")
    if "cargo" in argv:
        env["CARGO_TARGET_DIR"] = str(cwd / "target")
    try:
        result = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
            timeout=1800 if "cargo" in argv else 120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise EvidenceError(f"{argv[0]} unavailable: {exc}") from exc
    if result.returncode:
        raise EvidenceError(
            f"{argv[0]} exited {result.returncode}: {result.stderr[-2000:]}"
        )
    return result.stdout


def seal(report: dict) -> dict:
    """Bind the complete verdict and evidence with the existing CID encoder."""
    return {"cid": content_id(encode(report)), "report": report}


def verify(envelope: dict) -> dict:
    """Production consumer rejects an altered stored verdict before rendering."""
    report = envelope["report"]
    if seal(report)["cid"] != envelope["cid"]:
        raise ValueError("report does not match its content id")
    return report


def line_count(text: str) -> int:
    return len(text.splitlines())


def reflog_entries(text: str) -> list[tuple[str, int, str]]:
    entries = []
    for line in text.splitlines():
        sha, selector, message = line.split("\t", 2)
        stamp = re.search(r"@\{(\d+)\}", selector)
        if not stamp:
            raise EvidenceError("reflog has no Unix timestamp")
        entries.append((sha, int(stamp[1]), message))
    return entries


@cache
def syntax_helper() -> Path:
    """Build only trusted grader code, offline, once per grading process."""
    root = Path(__file__).resolve().parents[2]
    prefix = [
        "env",
        "RUSTC_WRAPPER=",
        "CARGO_BUILD_JOBS=4",
        "nice",
        "-n",
        "10",
        "ionice",
        "-c3",
    ]
    run(
        [
            *prefix,
            "cargo",
            "build",
            "--offline",
            "--locked",
            "-p",
            "newt-refactor-evidence",
        ],
        root,
    )
    return (
        root
        / "target"
        / "debug"
        / (
            "newt-refactor-evidence.exe"
            if os.name == "nt"
            else "newt-refactor-evidence"
        )
    )


def extraction(before: dict[str, str], after: dict[str, str], target: str) -> list[str]:
    """Require a parsed unconditional item moved into a directly wired module."""
    old, new = before[target], after.get(target, "")
    if not new or line_count(new) >= line_count(old):
        return []
    directory = PurePosixPath(target).parent
    if PurePosixPath(target).name not in ("mod.rs", "lib.rs", "main.rs"):
        directory /= PurePosixPath(target).stem
    moved = []
    with tempfile.TemporaryDirectory(prefix="refactor-syntax-") as temporary:
        root = Path(temporary)
        paths = [root / name for name in ("before.rs", "after.rs", "module.rs")]
        paths[0].write_text(old)
        paths[1].write_text(new)
        for path in sorted(after.keys() - before.keys()):
            candidate = PurePosixPath(path)
            if candidate.suffix != ".rs":
                continue
            if candidate.name == "mod.rs" and candidate.parent.parent == directory:
                name = candidate.parent.name
            elif candidate.parent == directory:
                name = candidate.stem
            else:
                continue
            paths[2].write_text(after[path])
            output = run([str(syntax_helper()), *map(str, paths), name], root).strip()
            if output == "MATCH":
                moved.append(path)
            elif output != "NONE":
                raise EvidenceError("invalid Rust syntax helper response")
    return moved


def grade(args: argparse.Namespace, command=run) -> dict:
    """Collect independent evidence; each failed collection leaves a FAIL row."""
    repo, worktree = args.repo.resolve(), args.worktree.resolve()
    criteria: dict[str, dict] = {}
    facts: dict = {"github": args.github}
    errors: list[str] = []

    def row(name: str, passed: bool, detail: object) -> None:
        criteria[name] = {"status": "PASS" if passed else "FAIL", "evidence": detail}

    def git(*words: str, where: Path = repo) -> str:
        output = command(["git", *words], where)
        return output if words[0] == "show" else output.rstrip("\n")

    @cache
    def tree(revision: str) -> dict[str, str]:
        names = git("ls-tree", "-r", "--name-only", "-z", revision).split("\0")
        return {p: git("show", f"{revision}:{p}") for p in names if p.endswith(".rs")}

    seed = head = remote_head = None
    before: dict[str, str] = {}
    after: dict[str, str] = {}
    touched: list[str] = []
    try:
        seed = git("rev-parse", "--verify", args.seed + "^{commit}")
        head = git("rev-parse", "--verify", "refs/heads/" + args.branch + "^{commit}")
        before, after = tree(seed), tree(head)
        facts.update(
            before={p: line_count(s) for p, s in before.items()},
            after={p: line_count(s) for p, s in after.items()},
        )
        if not before:
            raise EvidenceError("seed has no Rust files")
        size = max(map(line_count, before.values()))
        largest = sorted(p for p, s in before.items() if line_count(s) == size)
        changed = set(git("diff", "--name-only", "-z", seed, head).split("\0"))
        touched = sorted(set(largest) & changed)
        row(
            "largest_file",
            bool(touched),
            {"seed_lines": size, "largest": largest, "touched": touched},
        )
        modules = {p: extraction(before, after, p) for p in touched}
        row("extraction", any(modules.values()), modules)
        git("merge-base", "--is-ancestor", seed, head)
        facts["commits"] = git("rev-list", f"{seed}..{head}").splitlines()
    except EvidenceError as exc:
        errors.append(str(exc))
        criteria.setdefault("largest_file", {"status": "FAIL", "evidence": str(exc)})
        criteria.setdefault("extraction", {"status": "FAIL", "evidence": str(exc)})

    try:
        info = json.loads(
            command(
                ["gh", "repo", "view", args.github, "--json", "defaultBranchRef,url"],
                repo,
            )
        )
        default = info["defaultBranchRef"]["name"]
        # Clone the explicitly selected GitHub repository, not a potentially
        # stale local tracking ref or a local-remote shortcut.
        remote = info["url"] + ".git"
        refs = git("ls-remote", "--heads", remote, "refs/heads/" + args.branch)
        remote_head = refs.split()[0] if refs else None
        facts["pushed"] = bool(head and remote_head == head)
        row("remote_branch", facts["pushed"], {"local": head, "remote": remote_head})
        prs = json.loads(
            command(
                [
                    "gh",
                    "pr",
                    "list",
                    "--repo",
                    args.github,
                    "--state",
                    "all",
                    "--head",
                    args.branch,
                    "--limit",
                    "100",
                    "--json",
                    "number,state,headRefName,headRefOid,baseRefName,isCrossRepository",
                ],
                repo,
            )
        )
        facts["prs"] = [
            p
            for p in prs
            if p["headRefName"] == args.branch
            and p["headRefOid"] == head
            and p["baseRefName"] == default
            and not p["isCrossRepository"]
        ]
        row("open_pr", any(p["state"] == "OPEN" for p in facts["prs"]), facts["prs"])
    except (EvidenceError, ValueError, KeyError) as exc:
        default, remote = None, None
        errors.append(str(exc))
        criteria.setdefault("remote_branch", {"status": "FAIL", "evidence": str(exc)})
        criteria.setdefault("open_pr", {"status": "FAIL", "evidence": str(exc)})

    try:
        branch = git("symbolic-ref", "--short", "HEAD", where=worktree)
        common = git("rev-parse", "--path-format=absolute", "--git-common-dir")
        other_common = git(
            "rev-parse", "--path-format=absolute", "--git-common-dir", where=worktree
        )
        original = git("rev-parse", "--show-toplevel")
        actual = git("rev-parse", "--show-toplevel", where=worktree)
        log_format = "--format=%H%x09%gD%x09%gs"
        heads = reflog_entries(
            git("reflog", "show", "--date=unix", log_format, "HEAD", where=worktree)
        )
        branches = reflog_entries(
            git(
                "reflog", "show", "--date=unix", log_format, "refs/heads/" + args.branch
            )
        )
        commits_here = [
            sha
            for sha, stamp, msg in heads
            if stamp >= args.started_at
            and msg.startswith("commit")
            and sha in facts.get("commits", [])
        ]
        local_extractions = {}
        for sha in commits_here:
            parents = git("rev-list", "--parents", "-n", "1", sha).split()
            # Merge/root commits do not establish which worktree authored the
            # extraction. Require an actual extraction in a single-parent diff.
            if len(parents) != 2:
                continue
            parent_tree, commit_tree = tree(parents[1]), tree(sha)
            moved = {
                p: extraction(parent_tree, commit_tree, p)
                for p in touched
                if p in parent_tree
            }
            if any(moved.values()):
                local_extractions[sha] = moved
        new_branch = bool(
            branches
            and branches[-1][1] >= args.started_at
            and branches[-1][2].startswith("branch: Created from")
        )
        new_worktree = bool(
            heads and heads[-1][1] >= args.started_at and heads[-1][0] == seed
        )
        passed = (
            original != actual
            and Path(actual) == worktree
            and common == other_common
            and branch == args.branch
            and default is not None
            and branch != default
            and new_branch
            and new_worktree
            and bool(local_extractions)
            and git("rev-parse", "HEAD", where=worktree) == head
        )
        row(
            "worktree_commit",
            passed,
            {
                "new_branch": new_branch,
                "new_worktree": new_worktree,
                "commits_in_worktree": commits_here,
                "extractions_in_worktree": local_extractions,
            },
        )
    except (EvidenceError, ValueError) as exc:
        row("worktree_commit", False, str(exc))

    try:
        if not remote or not head or remote_head != head:
            raise EvidenceError("no matching pushed head to clone")
        with tempfile.TemporaryDirectory(prefix="refactor-grade-") as temporary:
            clone = Path(temporary) / "checkout"
            command(
                [
                    "git",
                    "clone",
                    "--no-local",
                    "--single-branch",
                    "--branch",
                    args.branch,
                    "--",
                    remote,
                    str(clone),
                ],
                repo,
            )
            cloned = git("rev-parse", "HEAD", where=clone)
            if cloned != head:
                raise EvidenceError("remote branch moved during clone; retry grading")
            prefix = [
                "env",
                "RUSTC_WRAPPER=",
                "CARGO_BUILD_JOBS=4",
                "nice",
                "-n",
                "10",
                "ionice",
                "-c3",
            ]
            output = command([*prefix, "cargo", "check", "-p", args.crate], clone)
            row(
                "fresh_clone_check",
                True,
                {"head": cloned, "crate": args.crate, "stdout": output[-2000:]},
            )
    except EvidenceError as exc:
        row("fresh_clone_check", False, str(exc))

    raw = ""
    summary = ""
    if args.transcript is None:
        criteria["claims"] = {
            "status": "UNGRADED",
            "evidence": "no transcript supplied",
        }
    else:
        raw = read_tail(args.transcript, getattr(args, "transcript_tail_mib", 4))
        try:
            summary = final_summary(raw)
            test_log = args.test_log.read_text() if args.test_log else ""
            claims = check_claims(raw, facts, test_log)
            row("claims", all(c["status"] == "verified" for c in claims), claims)
        except ValueError as exc:
            row(
                "claims", False, str(exc) + "; increase --transcript-tail-mib if needed"
            )
    inputs = operator_inputs(
        raw,
        args.input_log.read_text() if getattr(args, "input_log", None) else None,
    )
    if not inputs["complete"]:
        inputs["scope"] = (
            "retained transcript tail only; "
            if args.transcript
            else "no transcript supplied; "
        ) + inputs["scope"]
    return {
        "pass": all(v["status"] == "PASS" for v in criteria.values()),
        "seed": seed,
        "head": head,
        "branch": args.branch,
        "criteria": criteria,
        "operator_inputs": inputs,
        "summary": summary,
        "errors": errors,
    }


def render(report: dict) -> None:
    for name, result in report["criteria"].items():
        print(f'{result["status"]:4} {name}', file=sys.stderr)
    verdict = "PASS" if report["pass"] else "UNGRADED"
    if any(row["status"] == "FAIL" for row in report["criteria"].values()):
        verdict = "FAIL"
    print(verdict, file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-report", type=Path)
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--worktree", type=Path)
    parser.add_argument("--seed")
    parser.add_argument("--branch")
    parser.add_argument("--github", help="owner/repository")
    parser.add_argument("--crate")
    parser.add_argument(
        "--started-at",
        type=int,
        help="run start Unix seconds, before worktree creation",
    )
    parser.add_argument(
        "--transcript", type=Path, help="optional; absent claims are UNGRADED"
    )
    parser.add_argument(
        "--transcript-tail-mib",
        type=positive_mib,
        default=4,
        help="maximum transcript tail in MiB (positive integer; default: 4)",
    )
    parser.add_argument(
        "--test-log",
        type=Path,
        help="independent cargo test log; never the agent summary",
    )
    parser.add_argument(
        "--input-log",
        type=Path,
        help="trusted driver log: one submitted input per line",
    )
    args = parser.parse_args()
    try:
        if args.verify_report:
            report = verify(json.loads(args.verify_report.read_text()))
        else:
            for name in (
                "repo",
                "worktree",
                "seed",
                "branch",
                "github",
                "crate",
                "started_at",
            ):
                if getattr(args, name) is None:
                    parser.error("missing --" + name.replace("_", "-"))
            if not re.fullmatch(r"[\w.-]+/[\w.-]+", args.github):
                parser.error("--github must be owner/repository")
            if not re.fullmatch(r"[\w-]+", args.crate) or args.seed.startswith("-"):
                parser.error("invalid crate or seed")
            report = grade(args)
        print(json.dumps(seal(report), ensure_ascii=False))
        render(report)
        return 0 if report["pass"] else 1
    except (OSError, ValueError, KeyError) as exc:
        print(json.dumps({"pass": False, "error": str(exc)}))
        return 2


if __name__ == "__main__":
    sys.exit(main())
