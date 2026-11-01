#!/usr/bin/env python3
"""Regression for the placeholder-identity incident: 22 commits on main were
authored as `sim <x@y>`, and GitHub credited an unrelated account for them.
check_commit_identity.sh must reject such emails and accept real ones;
check_session_url.sh must keep blocking session links.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
IDENTITY = SCRIPTS / "check_commit_identity.sh"
SESSION = SCRIPTS / "check_session_url.sh"
HOOK = SCRIPTS.parent / ".githooks" / "commit-msg"


def run(*args, stdin=""):
    return subprocess.run(
        [str(a) for a in args], input=stdin, capture_output=True, text=True
    ).returncode


@unittest.skipUnless(os.name == "posix", "the checkers require Bash")
class CommitIdentityTests(unittest.TestCase):
    def test_real_emails_pass(self):
        for email in (
            "123+someone@users.noreply.github.com",
            "noreply@anthropic.com",
            "dev@example.org",
            "a.b@sub.domain.io",
        ):
            with self.subTest(email=email):
                self.assertEqual(run(IDENTITY, email), 0)

    def test_placeholder_emails_fail(self):
        for email in ("x@y", "a@localhost", "test@example", "", "nobody",
                      "a@b.", "a@.b", "@example.com", "a b@c.d"):
            with self.subTest(email=email):
                self.assertEqual(run(IDENTITY, email), 1)

    def test_one_bad_email_among_good_fails(self):
        self.assertEqual(run(IDENTITY, "noreply@anthropic.com", "x@y"), 1)

    def test_session_url_blocked(self):
        self.assertEqual(run(SESSION, "-", stdin="fix\n\nClaude-Session: abc\n"), 1)
        self.assertEqual(
            run(SESSION, "-", stdin="see https://claude.ai/code/session_x\n"), 1
        )

    def test_clean_message_passes(self):
        msg = "fix\n\nCo-Authored-By: Claude <noreply@anthropic.com>\n"
        self.assertEqual(run(SESSION, "-", stdin=msg), 0)


@unittest.skipUnless(os.name == "posix", "the hook requires Bash and git")
class CommitMsgHookTests(unittest.TestCase):
    """The hook reads the identity git is about to record."""

    def commit(self, email):
        with tempfile.TemporaryDirectory(prefix="newt-commit-identity-") as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            for script in (IDENTITY, SESSION):
                (root / "scripts" / script.name).write_bytes(script.read_bytes())
                (root / "scripts" / script.name).chmod(0o755)
            env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
            env.update(
                GIT_CONFIG_NOSYSTEM="1",
                GIT_CONFIG_GLOBAL=os.devnull,
                GIT_AUTHOR_NAME="sim",
                GIT_AUTHOR_EMAIL=email,
                GIT_COMMITTER_NAME="sim",
                GIT_COMMITTER_EMAIL=email,
            )
            git = ["git", "-C", tmp]
            subprocess.run(git + ["init", "-q", "--template="], env=env, check=True)
            subprocess.run(
                git + ["config", "core.hooksPath", str(HOOK.parent)], env=env, check=True
            )
            return subprocess.run(
                git + ["commit", "-q", "--allow-empty", "-m", "fixture"],
                env=env, capture_output=True, text=True,
            ).returncode

    def test_placeholder_identity_blocks_commit(self):
        self.assertNotEqual(self.commit("x@y"), 0)

    def test_real_identity_commits(self):
        self.assertEqual(self.commit("fixture@example.invalid"), 0)


if __name__ == "__main__":
    unittest.main()
