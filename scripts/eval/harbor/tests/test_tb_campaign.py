"""tb_campaign.py: claims are read from each harness's own log, an unreadable
claim stays unknown instead of becoming "not claimed", and a trial that ran
another model or was never graded stays visible in the summary.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest discover scripts/eval/harbor/tests
"""

import json
import tomllib
import unittest
from pathlib import Path

from pi_log import pi_inference_failure, pi_session_claim

from tb_campaign import (
    PINNED, awaiting, build_cell, ceiling_reached, codex_claim, deadline_cancels, error_cause, fingerprint,
    harness_evidence, is_trial_config, job_state, ledger_entry, ledger_total_s, load_treatment, newcombe,
    newt_claim, observed, pair, pair_report, parse_schedule, pi_claim, pin_extend, pin_mismatch, refusal,
    render_profile, summarize, treatment_env, trial_plan, wilson, window_at,
)

HARBOR = Path(__file__).resolve().parent.parent
BASE = ('default_backend = "b"\n[[backends]]\nname = "b"\n'
        'endpoint = "http://inference.invalid:8080"\nmodel = "old"\nkind = "openai"\n')


def jl(*records):
    return [json.dumps(r) for r in records]


def row(**kw):
    base = dict(state="graded", reward=0.0, resolved=False, error_cause=None, inference_failure=None,
                claimed_done=True, exception=None, model_effective="m", tokens_in=None, tokens_out=None,
                agent_s=None, max_request_output_tokens=None)
    return {**base, **kw}


class Claims(unittest.TestCase):
    def test_newt_uses_the_last_contract_outcome(self):
        lines = jl({"kind": "solve_result", "status": "completed"}, {"outcome": "timeout"})
        self.assertEqual(newt_claim(lines), (False, "contract outcome=timeout, solve_result status=completed"))
        self.assertIsNone(newt_claim(jl({"kind": "chat_completion_finish"})))

    def test_newt_claims_done_only_when_outcome_and_status_are_both_completed(self):
        # #2315: RepairExhausted / VerificationIncomplete map to outcome completed but
        # status incomplete. newt itself says "not done", so that is not a claim.
        incomplete = jl({"kind": "solve_result", "status": "incomplete"}, {"outcome": "completed"})
        self.assertEqual(newt_claim(incomplete)[0], False)
        done = jl({"kind": "solve_result", "status": "completed"}, {"outcome": "completed"})
        self.assertEqual(newt_claim(done)[0], True)

    def test_pi_claims_only_on_a_final_stop(self):
        def end(reason):
            return {"type": "agent_end", "messages": [
                {"role": "assistant", "stopReason": "toolUse"}, {"role": "user"},
                {"role": "assistant", "stopReason": reason}]}

        self.assertEqual(pi_claim(jl(end("stop")))[0], True)
        self.assertEqual(pi_claim(jl(end("length")))[0], False)
        self.assertIsNone(pi_claim(jl({"type": "turn_end"}) + ["not json"]))
        # auto-retry: an errored first attempt, then a finished one — the last agent_end decides
        self.assertEqual(pi_claim(jl(end("error"), end("stop")))[0], True)

    def test_codex_turn_outcome(self):
        self.assertEqual(codex_claim(jl({"type": "turn.started"}, {"type": "turn.completed"}))[0], True)
        self.assertEqual(codex_claim(jl({"type": "turn.failed"}))[0], False)
        self.assertIsNone(codex_claim(jl({"type": "thread.started"})))


class ErrorCause(unittest.TestCase):
    NZ = "NonZeroAgentExitCodeError"

    def test_runaway_truncated_stream_after_replies_is_agent(self):
        # circuit-fibsqrt__sQVNJH6 (smart-ab-8 baseline): 13 completions, then newt refused the stream.
        lines = jl(*[{"kind": "chat_completion_finish", "finish_reason": "tool_calls"}] * 13,
                   {"kind": "solve_result", "error": "streamed tool batch did not finish with tool_calls"})
        replies, err = harness_evidence("newt", lines)
        self.assertEqual(replies, 13)
        self.assertEqual(error_cause(self.NZ, 1, err, None, replies), "agent")

    def test_apparatus_evidence_is_infra_even_after_replies(self):
        self.assertEqual(error_cause(None, None, None, "auto_retry_end success=false: Connection error.", 0), "infra")
        self.assertEqual(error_cause(self.NZ, 1, "inference endpoint: error sending request", None, 4), "infra")
        self.assertEqual(error_cause(self.NZ, 137, None, None, 9), "infra")
        self.assertEqual(error_cause("EnvironmentStartTimeoutError", None, None, None, 0), "infra")
        _, err = harness_evidence("codex", jl({"type": "turn.failed", "error": {"message": "stream disconnected before completion"}}))
        self.assertEqual(error_cause(self.NZ, 1, err, None, 2), "infra")

    def test_unprovable_is_unknown_and_no_exception_is_none(self):
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 0), "unknown")
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 5), "agent")
        self.assertIsNone(error_cause(None, None, None, None, 7))


class Summary(unittest.TestCase):
    def test_wilson(self):
        lo, hi = wilson(0, 1)
        self.assertEqual(lo, 0.0)
        self.assertAlmostEqual(hi, 0.7935, places=3)
        self.assertIsNone(wilson(0, 0))

    def test_unknown_claims_and_missing_grades_are_not_folded_in(self):
        rows = [
            row(resolved=True, reward=1.0),                                   # true completion
            row(),                                                            # false completion
            row(claimed_done=False, resolved=True, reward=1.0),               # false incomplete
            row(claimed_done=None),                                           # unrecoverable claim
            row(state="error", reward=None, exception="AgentSetupTimeoutError", error_cause="infra"),
            row(model_effective="other"),                                     # ran another model
            row(state="error", error_cause="infra", inference_failure="auto_retry_end"),  # pi exit 0
            row(state="error", claimed_done=False, exception="AgentTimeoutError", error_cause="agent",
                max_request_output_tokens=96345),
        ]
        s = summarize({"expected": 9, "model": "m"}, rows)
        self.assertEqual((s["expected"], s["observed"], s["graded"], s["errors"]), (9, 8, 5, 3))
        self.assertEqual(s["claimed"], 3)  # infra trials never count as a claim outcome
        self.assertEqual((s["false_completions"], s["false_incompletes"]), (2, 1))
        self.assertEqual((s["unrecoverable_claims"], s["exceptions"], s["model_mismatches"]), (1, 2, 1))
        self.assertEqual((s["inference_errors"], s["agent_timeouts"], s["max_request_output"]), (1, 1, 96345))
        self.assertEqual(summarize({"expected": 1, "model": "m"}, [row(harness_outcome="completed", harness_status="incomplete",
                                                                       claimed_done=False)])["terminal_not_done"], 1)
        self.assertEqual((s["rate_excl"], s["rate_agent_fail"], s["causes"]), ((2, 5), (2, 6), (2, 1, 0)))
        self.assertEqual(s["tokens_in"], (0, 0))  # none known: count 0, not a zero cost

    def test_two_resolve_rates(self):
        rows = [
            row(resolved=True, reward=1.0),
            row(),
            row(state="error", exception="NonZeroAgentExitCodeError", error_cause="agent"),  # runaway, reward 0
            row(state="error", exception="NetworkConnectionError", error_cause="infra"),     # router down
            row(state="error", exception="AgentTimeoutError", error_cause="unknown"),
        ]
        s = summarize({"expected": 5, "model": "m"}, rows)
        self.assertEqual(s["rate_excl"], (1, 2))
        self.assertEqual(s["rate_agent_fail"], (1, 3))
        self.assertEqual(s["causes"], (1, 1, 1))
        self.assertEqual(s["tokens_in"], (0, 0))  # none known: count 0, not a zero cost


class Treatments(unittest.TestCase):
    def test_none_is_the_baseline(self):
        t = load_treatment("none")
        self.assertEqual((t["name"], t["sha256"], t["env"], t["expect"]), ("none", None, {}, {}))
        self.assertEqual(render_profile(t, "m", BASE), BASE.replace('model = "old"', 'model = "m"'))

    def test_committed_treatments_render_to_valid_profiles_without_placeholders(self):
        files = sorted((HARBOR / "treatments").glob("*.toml"))
        self.assertTrue(files)
        for f in files:
            text = render_profile(load_treatment(f), "qwen3-coder_30b", BASE)
            self.assertNotIn("{{", text, f)
            self.assertEqual(tomllib.loads(text)["backends"][0]["model"], "qwen3-coder_30b", f)

    def test_smart_takes_its_endpoint_from_the_local_profile(self):
        doc = tomllib.loads(render_profile(load_treatment(HARBOR / "treatments/smart.toml"), "m", BASE))
        self.assertEqual(doc["smart_harness"]["backend"], {"endpoint": "http://inference.invalid:8080", "model": "m", "kind": "openai"})

    def test_env_outside_the_adapter_knobs_is_refused(self):
        with self.assertRaises(ValueError):
            load_treatment(HARBOR / "tests/fixtures/treatment-bad-env.toml")

    def test_requires_reaches_the_adapter_and_none_requires_nothing(self):
        t = load_treatment(HARBOR / "tests/fixtures/treatment-requires-scratchpad.toml")
        self.assertEqual(treatment_env(t), {"NEWT_BENCH_REQUIRE_FEATURES": "scratchpad"})
        self.assertEqual(treatment_env(load_treatment("none")), {})
        with self.assertRaises(ValueError):  # names are the receipt keys: scratchpad, code_search, crew
            load_treatment(HARBOR / "tests/fixtures/treatment-requires-unknown.toml")

    def test_the_verify_outcomes_treatment_reaches_the_adapter_and_is_observable(self):
        # #2315: the result-aware switch is a treatment knob, and the arm is
        # confirmed from the receipt, not assumed from the declared env.
        t = load_treatment(HARBOR / "treatments/verify-outcomes.toml")
        self.assertEqual(treatment_env(t), {"NEWT_BENCH_SELF_VERIFY": "1", "NEWT_BENCH_VERIFY_OUTCOMES": "1"})
        treated = {"receipt": {"verification": {"mode": "result_aware", "repair_allowance": 3}}}
        self.assertIs(observed(t["expect"], treated), True)
        self.assertIs(observed(t["expect"], {"receipt": {"verification": {"mode": "off"}}}), False)

    def test_the_campaign_unsets_every_arm_changing_knob_it_does_not_set(self):
        # An exported switch must not silently flip baseline cells.
        script = (HARBOR / "tb-campaign.sh").read_text()
        unset = next(line for line in script.splitlines() if line.startswith("unset "))
        for knob in ("NEWT_BENCH_SMART", "NEWT_BENCH_VERIFY_OUTCOMES"):
            self.assertIn(knob, unset.split(), knob)

    def test_a_refused_requirement_is_read_from_the_exit_message(self):
        message = ("Command failed (exit 1): newt solve ...\nstdout: None\n"
                   "stderr: Error: required feature `scratchpad` is unavailable: no --scratchpad-state was supplied")
        # Wording from Feature::absence in newt-cli/src/solve_contract.rs (#2356).
        self.assertEqual(refusal(message),
                         "required feature `scratchpad` is unavailable: no --scratchpad-state was supplied")
        self.assertIsNone(refusal("Command failed (exit 1): streamed tool batch did not finish with tool_calls"))
        refused = [row(state="refused", claimed_done=True), row(state="refused", claimed_done=None)]
        s = summarize({"expected": 2, "model": "m"}, refused)  # never reached the model: no grade, no claim
        self.assertEqual((s["graded"], s["claimed"], s["unrecoverable_claims"], s["rate_agent_fail"]), (0, 0, 0, (0, 0)))

    def test_observed_reads_dotted_contract_paths(self):
        expect = {"effective_config.smart_harness": "*", "effective_config.ocap": "off"}
        on = {"outcome": "completed", "effective_config": {"smart_harness": {"enabled": True}, "ocap": "off"}}
        self.assertIs(observed(expect, on), True)
        self.assertIs(observed(expect, {"effective_config": {"ocap": "off"}}), False)
        self.assertIsNone(observed({}, on))       # nothing declared observable
        self.assertIsNone(observed(expect, None))  # no contract record to read

    def test_cell_binding_parses_json_and_counts(self):
        t = load_treatment(HARBOR / "treatments/smart.toml")
        cell = build_cell(t, ["expected", "24", "model_fingerprint_json", '{"size": 1}', "skipped", ""])
        self.assertEqual((cell["expected"], cell["model_fingerprint"], cell["skipped"]), (24, {"size": 1}, None))
        self.assertEqual((cell["treatment"], cell["treatment_env"]), ("smart", {"NEWT_BENCH_SMART": "1"}))
        self.assertEqual(len(cell["treatment_sha256"]), 64)


class Pinning(unittest.TestCase):
    CELL = dict(model="m", harness="newt", task_set_sha256="t", engine="e", ctx_served="131072",
                newt_binary_sha256="b", instrument_commit="c", model_fingerprint={"size": 1, "ftype": "Q4_K"},
                harness_version="0.8.0")

    def test_the_writing_cell_matches_its_own_pin(self):
        self.assertEqual(pin_mismatch(pin_extend({}, dict(self.CELL)), self.CELL), [])

    def test_every_pinned_field_refuses_a_different_cell(self):
        pin = pin_extend({}, dict(self.CELL))
        for k in PINNED:
            self.assertEqual(pin_mismatch(pin, {**self.CELL, k: "other"}), [k])

    def test_a_changed_model_fingerprint_refuses_naming_the_field(self):
        pin = pin_extend({}, dict(self.CELL))
        self.assertEqual(pin_mismatch(pin, {**self.CELL, "model_fingerprint": {"size": 2, "ftype": "Q4_K"}}),
                         ["model_fingerprint.size"])

    def test_a_changed_router_preset_thinking_flag_refuses(self):
        # The thinking switch lives in the router preset (identical for every harness);
        # a preset edit between cells must not pool with the cells before it.
        def served(kwargs):
            preset = "[m]\njinja = 1\nctx-size = 131072\nmodel = /x/y/m.gguf\n" + (f"chat-template-kwargs = {kwargs}\n" if kwargs else "")
            return {"id": "m", "meta": {"size": 1, "ftype": "Q8_0"},
                    "status": {"value": "loaded", "args": ["--ctx-size", "131072"], "preset": preset}}

        off = fingerprint(served('{"enable_thinking": false}'))
        self.assertEqual(off["chat_template_kwargs"], {"enable_thinking": False})
        self.assertEqual(off["gguf"], "m.gguf")  # from the preset when the args carry no --model
        self.assertIsNone(fingerprint(served(None))["chat_template_kwargs"])  # absent is recorded, not assumed
        pin = pin_extend({}, {**self.CELL, "model_fingerprint": off})
        on = {**self.CELL, "model_fingerprint": fingerprint(served('{"enable_thinking": true}'))}
        self.assertEqual(pin_mismatch(pin, on), ["model_fingerprint.chat_template_kwargs"])

    def test_a_new_model_extends_the_pin_and_harness_versions_are_pinned_once_seen(self):
        pin = pin_extend({}, dict(self.CELL))
        second = {**self.CELL, "model": "m2", "model_fingerprint": {"size": 9}}
        self.assertEqual(pin_mismatch(pin, second), [])
        pin_extend(pin, second)
        self.assertEqual(pin["models"]["m2"], {"size": 9})
        pi = {**self.CELL, "harness": "pi", "harness_version": None}   # before its first install
        self.assertEqual(pin_mismatch(pin, pi), [])
        pin_extend(pin, {**pi, "harness_version": "0.85.1"})
        self.assertEqual(pin_mismatch(pin, {**pi, "harness_version": "0.86.0"}), ["harness_version"])


def arm(outcomes, **kw):
    """Rows for one arm from {task: [resolved?, ...]}."""
    return [row(task=task, resolved=ok, reward=1.0 if ok else 0.0, **kw)
            for task, oks in outcomes.items() for ok in oks]


class Pairing(unittest.TestCase):
    TASKS = ("a", "b", "c", "d", "e")

    def test_newcombe_matches_a_hand_computed_value(self):
        lo, hi = newcombe(3, 12, 0, 12)
        self.assertAlmostEqual(lo, -0.0411, places=3)
        self.assertAlmostEqual(hi, 0.5323, places=3)

    def test_a_uniform_effect_has_a_tight_interval(self):
        p = pair(arm({t: [False] * 3 for t in self.TASKS}), arm({t: [True] * 3 for t in self.TASKS}), "excl")
        self.assertEqual((p["diff"], p["bootstrap"], p["better"], p["floor"]), (1.0, (1.0, 1.0), 5, False))

    def test_one_correlated_task_widens_the_clustered_interval_beyond_newcombe(self):
        # The treatment wins 3/3 on ONE task and nothing elsewhere: one cluster, not 3 independent wins.
        base = arm({t: [False] * 3 for t in self.TASKS})
        treat = arm({"a": [True] * 3, **{t: [False] * 3 for t in self.TASKS[1:]}})
        p = pair(base, treat, "excl")
        self.assertEqual((p["diff"], p["better"], p["tied"]), (0.2, 1, 4))
        self.assertEqual(p["bootstrap"][0], 0.0)
        self.assertGreater(p["bootstrap"][1], p["newcombe"][1])

    def test_bootstrap_is_reproducible_and_seeded(self):
        base = arm({"a": [True, False, False], "b": [False] * 3, "c": [True] * 3, "d": [False, True, False]})
        treat = arm({"a": [True] * 3, "b": [False, True, False], "c": [True, True, False], "d": [False] * 3})
        self.assertEqual(pair(base, treat, "excl")["bootstrap"], pair(base, treat, "excl")["bootstrap"])

    def test_floor_ceiling_unpaired_and_too_few_tasks_are_named(self):
        p = pair(arm({"a": [False] * 3, "b": [False] * 3}), arm({"a": [False] * 3, "z": [False] * 3}), "excl")
        self.assertEqual((p["diff"], p["floor"], p["unpaired"]), (0.0, True, ["b", "z"]))
        self.assertIsNone(p["bootstrap"])  # 1 paired task: no between-task variance to resample
        self.assertTrue(pair(arm({"a": [True]}), arm({"a": [True]}), "excl")["ceiling"])

    def test_rate_definitions_choose_the_trials(self):
        base = arm({"a": [False, False]})
        treat = arm({"a": [True]}) + [row(task="a", state="error", error_cause="agent")]  # runaway, reward 0
        self.assertEqual(pair(base, treat, "excl")["treat"], (1, 1))
        self.assertEqual(pair(base, treat, "agent_fail")["treat"], (1, 2))

    def test_report_refuses_to_pair_across_a_skipped_or_off_pin_cell(self):
        cells = {
            "m__newt": {"job": "m__newt", "model": "m", "harness": "newt", "treatment": "none"},
            "m__newt__smart": {"job": "m__newt__smart", "model": "m", "harness": "newt", "treatment": "smart",
                               "pin_mismatch": ["model_fingerprint"]},
        }
        self.assertIn("Not paired", pair_report(cells, []))
        cells["m__newt__smart"].pop("pin_mismatch")
        rows = [dict(r, job="m__newt") for r in arm({"a": [False] * 3})] + \
               [dict(r, job="m__newt__smart") for r in arm({"a": [True] * 3})]
        report = pair_report(cells, rows)
        self.assertIn("Δ = 3/3 − 0/3 = +1.00", report)
        self.assertIn("task-clustered paired bootstrap 95% not reported (1 paired tasks; needs 5)", report)


class PerTrialJobs(unittest.TestCase):
    """One Harbor job per trial: Harbor has no stop-after-this-trial, deletes a
    result-less trial dir on resume, and SIGTERM cancels the in-flight trial,
    so the process boundary is the only clean trial boundary."""

    def test_a_trial_job_is_done_only_with_a_non_cancelled_result(self):
        self.assertEqual(job_state([]), "absent")
        self.assertEqual(job_state([None]), "partial")  # a trial dir without result.json
        self.assertEqual(job_state([{"exception_info": {"exception_type": "CancelledError"}}]), "partial")
        # An errored trial is a recorded attempt: never re-run it.
        self.assertEqual(job_state([{"exception_info": {"exception_type": "NonZeroAgentExitCodeError"}}]), "done")
        self.assertEqual(job_state([{"exception_info": None, "verifier_result": {"rewards": {"reward": 0.0}}}]), "done")

    def test_every_task_gets_its_first_attempt_before_any_second(self):
        # Cutting third trials first (the design card's cut order) needs this order.
        self.assertEqual(trial_plan(["a", "b"], 2), [("a", 1), ("b", 1), ("a", 2), ("b", 2)])

    def test_trial_configs_are_told_from_job_configs(self):
        self.assertTrue(is_trial_config({"task": {"path": "x"}, "trial_name": "t__1"}))
        self.assertFalse(is_trial_config({"jobs_dir": "/x", "datasets": []}))


class Windows(unittest.TestCase):
    """The campaign runs only inside declared windows; the inference box is the maintainer's
    outside them. One schedule file is the only source of the windows."""

    SCHEDULE = "# nights on weekdays, whole weekend\nmon,tue,wed,thu,fri 20:00 08:00\nsat,sun 00:00 24:00\n"

    def test_overnight_and_weekend_windows(self):
        from datetime import datetime
        sched = parse_schedule(self.SCHEDULE)
        wed_night = window_at(sched, datetime(2026, 9, 16, 23, 30))   # a Wednesday
        self.assertEqual(wed_night[1], datetime(2026, 9, 17, 8, 0))   # ends Thursday 08:00
        thu_early = window_at(sched, datetime(2026, 9, 17, 7, 59))    # still Wednesday's window
        self.assertEqual(thu_early, wed_night)
        self.assertIsNone(window_at(sched, datetime(2026, 9, 17, 12, 0)))  # Thursday midday: his
        sat = window_at(sched, datetime(2026, 9, 19, 15, 0))
        self.assertEqual(sat[1], datetime(2026, 9, 20, 0, 0))

    def test_a_malformed_schedule_line_refuses(self):
        with self.assertRaises(ValueError):
            parse_schedule("someday 20:00 08:00")

    def test_a_second_deadline_cancel_exhausts_the_attempt(self):
        # A runaway can hit the deadline every window; the second cancel is final.
        archived = ["task-a__a1.1789430000.deadline", "task-a__a2.1789430001", "task-b__a1.1789430002.deadline"]
        self.assertEqual(deadline_cancels(archived, "task-a__a1"), 1)
        self.assertEqual(deadline_cancels(archived, "task-a__a2"), 0)  # a crash archive is not a deadline cancel
        self.assertEqual(job_state([], exhausted=True), "done")


class GpuHourLedger(unittest.TestCase):
    """One content-addressed line per trial; the running wall total is checked
    against the model's GPU-hour ceiling before each new trial."""

    RECORD = dict(campaign="c", model="m", cell="m__newt", task="task-a", attempt=1, window="2026-09-19T00:00",
                  state="done", agent_s=240.0, wall_s=300.0)

    def test_an_entry_is_addressed_by_its_record(self):
        entry = ledger_entry(dict(self.RECORD))
        self.assertTrue(entry["cid"].startswith("b"))  # CIDv1 base32, via bench_scoreboard.trial_cid
        self.assertEqual(entry["cid"], ledger_entry(dict(self.RECORD))["cid"])
        self.assertNotEqual(entry["cid"], ledger_entry({**self.RECORD, "wall_s": 301.0})["cid"])

    def test_the_total_counts_wall_seconds_and_refuses_a_tampered_line(self):
        entries = [ledger_entry(dict(self.RECORD)), ledger_entry({**self.RECORD, "attempt": 2, "wall_s": 3300.0})]
        self.assertEqual(ledger_total_s(entries), 3600.0)
        tampered = [{**entries[0], "record": {**entries[0]["record"], "wall_s": 1.0}}]
        with self.assertRaises(ValueError):
            ledger_total_s(tampered)

    def test_no_new_trial_once_the_ceiling_is_reached(self):
        self.assertFalse(ceiling_reached(167.9 * 3600, 168))
        self.assertTrue(ceiling_reached(168 * 3600, 168))
        self.assertFalse(ceiling_reached(10**9, None))  # no ceiling declared


class PiSessionLog(unittest.TestCase):
    """Harbor pipes pi through a block-buffered grep (stdbuf applies only to tee),
    so a KILLED pi trial's pi.txt loses up to 4 KiB of final events. pi's own
    session JSONL is unbuffered: it decides pi's last events. Fixtures are the real
    tails of pypi-server__N4dSxtG (2026-09-14 baseline), whose last act was a
    foreground `python -m http.server` that never returned."""

    FIX = HARBOR / "tests" / "fixtures"

    def setUp(self):
        self.session = (self.FIX / "pi-session-blocked-on-tool.jsonl").read_text().splitlines()
        self.txt = (self.FIX / "pi-txt-truncated-tail.jsonl").read_text().splitlines()

    def test_blocked_on_its_own_tool_is_agent(self):
        self.assertEqual(awaiting("pi", self.txt, self.session), "tool")
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 14, "tool"), "agent")

    def test_a_request_that_never_got_a_reply_is_unknown_on_timeout(self):
        waiting_on_model = self.session[:-1]  # ends at a toolResult: the next reply never came
        self.assertEqual(awaiting("pi", [], waiting_on_model), "model")
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 13, "model"), "unknown")
        self.assertEqual(error_cause("NonZeroAgentExitCodeError", None, None, None, 13, "model"), "agent")  # timeouts only

    def test_the_session_log_wins_over_a_truncated_pi_txt(self):
        self.assertEqual(awaiting("pi", self.txt, []), "model")            # what the cut pi.txt alone says
        self.assertEqual(awaiting("pi", self.txt, self.session), "tool")  # what happened
        self.assertEqual(pi_inference_failure(self.txt, []), "no assistant message")  # the cut file misleads
        self.assertIsNone(pi_inference_failure(self.txt, self.session))
        self.assertEqual(pi_session_claim(self.session), (False, "session last assistant stopReason=toolUse"))

    def test_a_timeout_with_no_readable_log_is_unknown_for_every_harness(self):
        # newt writes newt-events.jsonl only after the turn ends, so a killed newt has no
        # log at all; pi and codex with no log are in the same position. Same evidence
        # standard for all three: no log, no attribution.
        for harness in ("newt", "pi", "codex"):
            waiting_on = awaiting(harness, [], [])
            replies, _ = harness_evidence(harness, [], [])
            self.assertIsNone(waiting_on, harness)
            self.assertEqual(error_cause("AgentTimeoutError", None, None, None, replies, waiting_on), "unknown", harness)

    def test_codex_last_event_says_what_it_waits_on(self):
        self.assertEqual(awaiting("codex", jl({"type": "turn.started"}, {"type": "item.started"}), []), "tool")
        self.assertEqual(awaiting("codex", jl({"type": "item.completed", "item": {"type": "command_execution"}}), []), "model")
        self.assertIsNone(awaiting("newt", jl({"kind": "chat_completion_finish"}), []))


if __name__ == "__main__":
    unittest.main()
