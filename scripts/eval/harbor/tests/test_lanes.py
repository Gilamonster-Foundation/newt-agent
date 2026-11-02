"""The newt TUI drive (newt_agent.py): what it types, what it refuses, and how it
reads newt's own lifecycle reports.

Terminal-Bench runs drive the TUI a person uses, so bench behaviour cannot drift
from it. The flow tests replay a lifecycle log against a fake environment.
Run: PYTHONPATH=scripts/eval/harbor:scripts/eval/harbor/tests/stubs \\
     python -m unittest scripts/eval/harbor/tests/test_lanes.py
"""

import asyncio
import shlex
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import newt_agent as na
from harbor.agents.installed.base import NonZeroAgentExitCodeError

KNOBS = ("_OCAP", "_SMART", "_REQUIRE_FEATURES", "_MAX_ROUNDS", "_SELF_VERIFY",
         "_VERIFY_OUTCOMES", "_MODEL_DIGEST", "_INITIATIVE", "_TENACITY")


class Knobs(unittest.TestCase):
    def setUp(self):
        self.saved = {k: getattr(na, k) for k in KNOBS}

    def tearDown(self):
        for k, v in self.saved.items():
            setattr(na, k, v)


class Command(Knobs):
    def test_off_lane_is_the_unsafe_host_exec_pair_without_ambient(self):
        argv = na._newt_argv()
        self.assertEqual(argv[:4], ["newt", "--no-splash", "--disable-ocap", "--full-access"])
        self.assertNotIn("--yolo", argv)
        self.assertNotIn("--ambient", argv)

    def test_posture_flags_follow_the_knobs(self):
        na._INITIATIVE, na._TENACITY = "decisive", "relentless"
        argv = na._newt_argv()
        self.assertIn(["--initiative", "decisive"], [argv[i:i + 2] for i in range(len(argv))])
        self.assertIn(["--tenacity", "relentless"], [argv[i:i + 2] for i in range(len(argv))])
        na._INITIATIVE, na._TENACITY = "", ""
        self.assertNotIn("--initiative", na._newt_argv())

    def test_pane_shell_outlives_newt_and_records_its_exit(self):
        """A pane shell that exits hangs up the terminal's foreground group,
        which would kill servers a task asked the agent to leave running."""
        cmd = na._start_command("/my work")
        self.assertIn("exec sleep infinity", cmd)
        self.assertIn("newt.exit", cmd)
        self.assertIn("-c '/my work'", cmd)
        shlex.split(cmd)  # the nested quoting still parses


class Refusals(Knobs):
    def test_headless_only_knobs_are_refused_not_ignored(self):
        for name, value in (("_OCAP", "on"), ("_SMART", "1"), ("_REQUIRE_FEATURES", "crew"),
                            ("_MAX_ROUNDS", "60")):
            for k in KNOBS:
                setattr(na, k, self.saved[k])
            na._OCAP = na._SMART = na._REQUIRE_FEATURES = na._MAX_ROUNDS = ""
            setattr(na, name, value)
            self.assertEqual(len(na._refusals()), 1, name)

    def test_a_refused_knob_stops_the_agent_before_harbor_builds_a_container(self):
        na._OCAP = "on"
        with self.assertRaises(ValueError):
            na.NewtAgent(Path("."))

    def test_the_campaign_defaults_pass(self):
        na._OCAP = na._SMART = na._REQUIRE_FEATURES = ""
        for rounds in ("", "40"):
            na._MAX_ROUNDS = rounds
            self.assertEqual(na._refusals(), [], rounds)


class PaneEnv(Knobs):
    def test_reports_go_to_the_shim_and_set_knobs_reach_the_pane(self):
        na._SELF_VERIFY, na._VERIFY_OUTCOMES, na._MODEL_DIGEST = "off", "1", "sha256:abc"
        env = na._pane_env()
        self.assertEqual((env["HERDR_ENV"], env["HERDR_BIN_PATH"]), ("1", na.HERDR_SHIM))
        self.assertNotIn("HERDR_SOCKET_PATH", env)  # a socket would win over the shim
        self.assertEqual((env["NEWT_SELF_VERIFY"], env["NEWT_VERIFY_OUTCOMES"]), ("0", "1"))
        self.assertEqual(env["NEWT_MODEL_DIGEST"], "sha256:abc")

    def test_unset_knobs_stay_absent_and_retries_stay_newts_own(self):
        na._SELF_VERIFY = na._VERIFY_OUTCOMES = na._MODEL_DIGEST = ""
        env = na._pane_env()
        for key in ("NEWT_SELF_VERIFY", "NEWT_VERIFY_OUTCOMES", "NEWT_MODEL_DIGEST", "NEWT_HTTP_MAX_RETRIES"):
            self.assertNotIn(key, env)


def report(state, message=None):
    tail = f" --message {message}" if message else ""
    return f"1 pane report-agent bench --source custom:newt --agent newt --state {state}{tail}\n"


class Lifecycle(unittest.TestCase):
    def test_states_keep_report_agent_lines_only(self):
        log = ("0 pane report-agent-session bench --source custom:newt --agent newt --agent-session-id s\n"
               + report("idle") + report("working", "thinking") + report("idle", "failed")
               + "9 pane release-agent bench --source custom:newt --agent newt\n")
        self.assertEqual(na._states(log), ["idle", "working:thinking", "idle:failed"])

    def test_stop_reasons(self):
        for state, stop in (("idle", "turn-ended"), ("idle:failed", "turn-failed"),
                            ("blocked", "waiting-for-a-person"), ("exited", "newt-exited")):
            self.assertEqual(na._stop_reason(state), stop)

    def test_probe_is_the_end_of_the_last_line(self):
        probe = na._probe("Do X.\n\n  Then   write /app/out.txt  \n\n")
        self.assertTrue("Then write /app/out.txt".endswith(probe))
        self.assertEqual(len(probe), 16)


class FakeEnv:
    """Plays newt's lifecycle log in chunks: the first when newt starts, the
    next on Enter, the rest one per driver poll after that. A chunk `exit N`
    is newt exiting with N. Writes `newt.exit` when /exit is pasted, noting
    how many reports had arrived."""

    def __init__(self, logs: Path, chunks):
        self.logs, self.chunks, self.commands = logs, list(chunks), []
        self.submitted, self.states_at_exit = False, None
        self.task_env_config = types.SimpleNamespace(workdir="/app")

    def emit(self):
        if not self.chunks:
            return
        chunk = self.chunks.pop(0)
        if chunk.startswith("exit "):
            (self.logs / "newt.exit").write_text(chunk.split()[1] + "\n")
            return
        with open(self.logs / na.LIFECYCLE, "a") as fh:
            fh.write(chunk)

    async def upload_file(self, source, target):
        self.commands.append(f"upload {target}")

    async def exec(self, command, **kwargs):
        self.commands.append(command)
        if "new-session" in command:
            self.emit()
        if "-b quit" in command:
            self.states_at_exit = len(na._states((self.logs / na.LIFECYCLE).read_text()))
            (self.logs / "newt.exit").write_text("0\n")
        elif command.endswith("Enter"):
            self.submitted = True
            self.emit()
        screen = "write /app/out.txt" if "capture-pane" in command else ""
        return types.SimpleNamespace(stdout=screen)


class Flow(unittest.TestCase):
    def drive(self, *chunks):
        real_sleep = asyncio.sleep

        async def poll(_seconds):
            if env.submitted:
                env.emit()
            await real_sleep(0.005)

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(na, "SETTLE_SEC", 0.2), \
                mock.patch.object(na.asyncio, "sleep", poll):
            logs = Path(tmp)
            env = FakeEnv(logs, chunks)
            context = types.SimpleNamespace(metadata=None)
            error = None
            try:
                # Bounded: a driver that misses an exit would otherwise wait forever.
                asyncio.run(asyncio.wait_for(na.NewtAgent(logs).run("Write /app/out.txt", env, context), 10))
            except NonZeroAgentExitCodeError as exc:
                error = exc
            return context.metadata, env, error

    def test_a_queued_turn_runs_before_exit(self):
        """A retry or an approved plan starts the next turn with no input from a
        person, so the first idle is not the end."""
        meta, env, error = self.drive(report("idle"), report("working") + report("idle"),
                                      report("working", "run_command"), report("idle"))
        self.assertIsNone(error)
        self.assertEqual((meta["stop"], meta["newt_exit"]), ("turn-ended", 0))
        self.assertEqual(env.states_at_exit, 5)
        typed = [c for c in env.commands if "paste-buffer" in c or "send-keys" in c]
        self.assertIn("paste-buffer -p", typed[0])  # bracketed: one submission
        self.assertIn("-b task", typed[0])
        self.assertTrue(typed[1].endswith("Enter"))
        self.assertIn("-b quit", typed[-1])
        self.assertFalse(any("C-c" in c for c in env.commands))

    def test_a_stop_for_a_person_is_cancelled_then_exited(self):
        meta, env, error = self.drive(report("idle"), report("working") + report("blocked"))
        self.assertIsNone(error)
        self.assertEqual(meta["stop"], "waiting-for-a-person")
        quit_at = next(i for i, c in enumerate(env.commands) if "-b quit" in c)
        self.assertTrue(any("C-c" in c for c in env.commands[:quit_at]))

    def test_newt_dying_mid_turn_is_graded_with_its_exit_status(self):
        """newt reports nothing on the way out; its exit status ends the wait
        instead of Harbor's agent timeout."""
        meta, env, error = self.drive(report("idle"), report("working"), "exit 101")
        self.assertEqual((meta["stop"], meta["newt_exit"]), ("newt-exited", 101))
        self.assertIsInstance(error, NonZeroAgentExitCodeError)
        self.assertRegex(str(error), r"exit (\d+)")  # what tb_campaign.py parses
        self.assertFalse(any("-b quit" in c for c in env.commands))

    def test_a_failed_last_turn_is_raised_for_harbor_to_record(self):
        meta, _, error = self.drive(report("idle"), report("working") + report("idle", "failed"))
        self.assertEqual(meta["stop"], "turn-failed")
        self.assertIsInstance(error, NonZeroAgentExitCodeError)


if __name__ == "__main__":
    unittest.main()
