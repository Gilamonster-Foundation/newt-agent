#!/usr/bin/env bash
# Behavioral grader for the loop-completion yardstick (next-loop-levers.md, #971).
#
# The #548 grader (grade-548.sh) asks "does the FEATURE work?". These levers
# instead fix the AGENTIC LOOP itself, so the question is different: "did the
# TURN reach a usable done state, or die the incident's death?" — a cap banner
# with empty salvage, or a dangling 'Let me ...' narration. We measure that
# behaviorally: drive a built `newt` through the verbatim yardstick prompts in
# an ISOLATED $HOME, then read the signals the yardstick's "Scoring a rerun"
# table names out of the run's OWN conversations.db / usage.jsonl / stderr.
#
#   PASS  ⇔  a plan ledger exists  AND  the ending is not a dangling narration
#            AND  the turn was not capped with empty salvage
#
# Fixture + scoring signals: docs/design/evidence/next-loop-levers-yardstick.md
#
# ISOLATION: newt roots its data dir at $HOME/.newt (newt-core config.rs
# home_dir() -> $HOME; store.rs DB_FILE = conversations.db). We drive with
# HOME=<throwaway> so the run writes its own conversations.db and NEVER touches
# the operator's real ~/.newt. The caller seeds <throwaway>/.newt/config.toml
# with the backend + incident knobs (loop-sweep.sh does that from the operator's
# LOCAL, uncommitted endpoint template — so no host lands in a committed file).
#
# SECURITY (RATCHET.md invariant): this script names no host. The backend
# endpoint lives only in the seeded config the caller provides.
#
# Usage:
#   grade-loop.sh <newt-binary> --home <throwaway-with-.newt/config.toml> \
#                 [--prompts <file>] [--workdir <dir>] [--label <s>] \
#                 [--timeout <secs>]
#   grade-loop.sh --self-test        # offline: fabricate DBs, assert the grades
#
# Output: one JSON result line on stdout; a human report on stderr;
# exit 0 iff PASS. A trial where nothing persisted (drive failed / backend
# unreachable) is an ERROR (exit 2), never a silent FAIL — honest trials only.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

# Default fixture: the fully-public Session B (#969 plan request) + a `continue`
# follow-up. Each line is one user turn; grade-loop appends `/exit`.
default_prompts() {
  cat <<'EOF'
Come up with a plan to fix this issue for me: https://github.com/Gilamonster-Foundation/newt-agent/issues/969
continue
EOF
}

# ---- the pure grader: read one run's $HOME/.newt, print JSON, return pass ----
# Depends only on sqlite3 + coreutils, so --self-test can exercise it with a
# fabricated DB and no newt / no backend.
grade_home() { # $1=home $2=label  -> JSON on stdout, human on stderr; 0=PASS
  local home="$1" label="${2:-loop}"
  local db="$home/.newt/conversations.db"
  local usage="$home/.newt/usage.jsonl"

  if [ ! -f "$db" ]; then
    printf '{"label":"%s","error":"no conversations.db"}\n' "$label"
    echo "ERROR: no conversations.db under $home/.newt (drive never ran?)" >&2
    return 2
  fi
  local conv
  conv="$(sqlite3 "$db" "SELECT id FROM conversations ORDER BY updated_at_claim DESC LIMIT 1;" 2>/dev/null)"
  if [ -z "$conv" ]; then
    printf '{"label":"%s","error":"no turns persisted"}\n' "$label"
    echo "ERROR: no conversation persisted (backend unreachable / permission-blocked?)" >&2
    return 2
  fi

  # One SQL round-trip for the countable signals. phantom_reaches / events are
  # JSON columns (json_array_length via SQLite's built-in JSON1).
  local row
  row="$(sqlite3 -noheader -separator '|' "$db" "
    SELECT
      (SELECT COUNT(*) FROM turns WHERE conversation_id='$conv'),
      (SELECT COALESCE(MAX(json_array_length(events)),0) FROM turns WHERE conversation_id='$conv'),
      (SELECT COUNT(*) FROM turns WHERE conversation_id='$conv'
         AND assistant LIKE '%reached the tool-call limit of%'),
      (SELECT COUNT(*) FROM turns WHERE conversation_id='$conv'
         AND assistant LIKE '%reached the tool-call limit of%'
         AND assistant LIKE '%Progress captured%'),
      (SELECT COALESCE(SUM(CASE WHEN phantom_reaches IS NULL OR phantom_reaches='[]'
             THEN 0 ELSE json_array_length(phantom_reaches) END),0)
         FROM turns WHERE conversation_id='$conv'),
      (SELECT CASE WHEN plan IS NULL OR plan='{}' OR plan='' THEN 0 ELSE 1 END
         FROM conversations WHERE id='$conv');
  " 2>/dev/null)"
  local turns max_events cap_hits cap_salvaged hallucinations plan_exists
  IFS='|' read -r turns max_events cap_hits cap_salvaged hallucinations plan_exists <<<"$row"

  # update_plan calls (round position lives in events; a count is enough to say
  # "the model engaged the plan tool at all").
  local update_plan_calls
  update_plan_calls="$(sqlite3 "$db" "SELECT events FROM turns WHERE conversation_id='$conv';" 2>/dev/null \
                        | grep -oc 'update_plan' || true)"

  # Ending shape: does the LAST turn dangle on a 'Let me ...' style narration?
  local last tail_lc dangling
  last="$(sqlite3 "$db" "SELECT substr(assistant,-200) FROM turns WHERE conversation_id='$conv' ORDER BY seq DESC LIMIT 1;" 2>/dev/null)"
  tail_lc="$(printf '%s' "$last" | tr '[:upper:]' '[:lower:]' | tr -d '\r')"
  if grep -qE "(let me|let's|i'll|i will|next, i|now i'll)[^.!?]*$" <<<"$tail_lc"; then
    dangling=1; else dangling=0
  fi

  # Derived verdicts.
  local empty_salvage=0
  [ "${cap_hits:-0}" -gt 0 ] && [ "${cap_salvaged:-0}" -eq 0 ] && empty_salvage=1
  local pass=false
  if [ "${plan_exists:-0}" -eq 1 ] && [ "$dangling" -eq 0 ] && [ "$empty_salvage" -eq 0 ]; then
    pass=true
  fi

  # Cross-check hallucinations against usage.jsonl if present (belt + braces).
  local usage_halluc="null"
  [ -f "$usage" ] && usage_halluc="$(grep -o '"hallucinations":[0-9]*' "$usage" 2>/dev/null \
      | awk -F: '{s+=$2} END{print s+0}')"

  printf '{"label":"%s","turns":%s,"max_events":%s,"cap_hit":%s,"cap_salvaged":%s,"empty_salvage":%s,"plan_ledger":%s,"update_plan_calls":%s,"dangling_narration":%s,"phantom_reaches":%s,"usage_hallucinations":%s,"pass":%s}\n' \
    "$label" "${turns:-0}" "${max_events:-0}" "${cap_hits:-0}" "${cap_salvaged:-0}" \
    "$empty_salvage" "${plan_exists:-0}" "${update_plan_calls:-0}" "$dangling" \
    "${hallucinations:-0}" "${usage_halluc:-null}" "$pass"

  {
    echo "  turns=$turns  max_events=$max_events  plan_ledger=$plan_exists  update_plan_calls=$update_plan_calls"
    echo "  cap_hit=$cap_hits  cap_salvaged=$cap_salvaged  empty_salvage=$empty_salvage  dangling=$dangling  phantom_reaches=$hallucinations"
    if [ "$pass" = true ]; then
      echo "RESULT[$label]: PASS — plan ledger present, non-empty ending, no dangling narration."
    else
      echo "RESULT[$label]: FAIL — $( [ "${plan_exists:-0}" -eq 0 ] && echo 'no plan ledger; ' )$( [ "$empty_salvage" -eq 1 ] && echo 'capped w/ empty salvage; ' )$( [ "$dangling" -eq 1 ] && echo 'dangling narration; ' )incident signature."
    fi
  } >&2

  [ "$pass" = true ]
}

# ---- drive: run the yardstick against an isolated HOME ----
run_yardstick() { # $1=newt $2=home $3=prompts $4=workdir $5=timeout
  local newt="$1" home="$2" prompts="$3" workdir="$4" timeout_s="$5"
  [ -x "$newt" ]                   || { echo "grade-loop: not executable: $newt" >&2; return 2; }
  [ -f "$home/.newt/config.toml" ] || { echo "grade-loop: seed $home/.newt/config.toml first (backend + incident knobs)" >&2; return 2; }
  command -v python3 >/dev/null    || { echo "grade-loop: python3 required (pty_drive.py)" >&2; return 2; }
  # Drive the interactive TUI FAITHFULLY via a PTY pacer (pty_drive.py): a blind
  # pipe can't — `newt --plain` needs a controlling terminal (ENXIO on a pipe).
  # Each prompt line is one user turn; append /exit. Details:
  #   HOME + --config-dir $home/.newt : load the seeded backend/knobs AND root
  #       the run's OWN conversations.db under $home (isolated from ~/.newt).
  #   NEWT_FULL_ACCESS + NO_PROMPT    : grant authority + non-interactive (no
  #       human at the pipe; also removes the yardstick's permission-latency
  #       confound). NB: this still leaves the exec shell CONFINED — a
  #       run_command WRITE is denied and the agent emits request_permissions
  #       instead of running it (read-only run_command is routed to built-ins,
  #       so it still works). For a fully unconfined host shell (how the
  #       incident agent operated) ALSO `export NEWT_DISABLE_OCAP=1` before
  #       running — grade-loop passes it through. It is deliberately NOT set by
  #       default: an unconfined agent loop is a footgun the operator opts into.
  #       (Needing BOTH full-access AND disable-ocap to get real work done is a
  #       newt rough edge worth fixing separately.) --no-splash skips the 0.7.1
  #       start screen. NOT --ephemeral, so the turns persist for the grader.
  local pf="$home/.yardstick-prompts"
  { cat "$prompts"; printf '/exit\n'; } > "$pf"
  # Set GRADE_LOOP_PACER_DEBUG=1 to log the pacer's prime/type/exit trace to
  # run.stderr.log (troubleshooting a "no turns persisted" drive).
  local dbg=(); [ -n "${GRADE_LOOP_PACER_DEBUG:-}" ] && dbg=(--debug)
  HOME="$home" NEWT_FULL_ACCESS=1 NEWT_NO_PROMPT_FOR_PERMISSIONS=1 \
    python3 "$HERE/pty_drive.py" --prompts "$pf" --workdir "$workdir" --timeout "$timeout_s" "${dbg[@]}" \
      -- "$newt" --config-dir "$home/.newt" --no-splash --plain \
      >"$home/run.stdout.log" 2>"$home/run.stderr.log"
  return 0
}

# ---- offline self-test: fabricate DBs, assert the grader's verdicts ----
self_test() {
  local tmp; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' RETURN
  local fail="$tmp/fail" pass="$tmp/pass"
  mkdir -p "$fail/.newt" "$pass/.newt"

  # FAIL fixture: no plan, capped, empty salvage, dangling 'Let me ...'.
  sqlite3 "$fail/.newt/conversations.db" "
    CREATE TABLE conversations(id TEXT, updated_at_claim INT, plan TEXT DEFAULT '{}');
    CREATE TABLE turns(conversation_id TEXT, seq INT, events TEXT, tokens_in INT, tokens_out INT, phantom_reaches TEXT, assistant TEXT);
    INSERT INTO conversations VALUES('c1',1,'{}');
    INSERT INTO turns VALUES('c1',1,'[1,2,3]',100,50,'[\"a\",\"b\"]','(reached the tool-call limit of 25 rounds; the final tools-disabled summary described future tool actions instead of final state)');
    INSERT INTO turns VALUES('c1',2,'[1]',80,40,'[]','Let me look at the OCAP reason field');
  "
  # PASS fixture: plan present, clean ending, no cap.
  sqlite3 "$pass/.newt/conversations.db" "
    CREATE TABLE conversations(id TEXT, updated_at_claim INT, plan TEXT DEFAULT '{}');
    CREATE TABLE turns(conversation_id TEXT, seq INT, events TEXT, tokens_in INT, tokens_out INT, phantom_reaches TEXT, assistant TEXT);
    INSERT INTO conversations VALUES('c2',1,'{\"steps\":[{\"t\":\"read\"},{\"t\":\"edit\"}]}');
    INSERT INTO turns VALUES('c2',1,'[1,2]',100,50,'[]','Here is the plan: 1. read the file 2. make the edit. Done.');
  "

  local rc=0
  echo "== self-test: FAIL fixture ==" >&2
  if grade_home "$fail" fail-fixture >/dev/null; then echo "SELF-TEST BUG: fail fixture graded PASS" >&2; rc=1; fi
  echo "== self-test: PASS fixture ==" >&2
  if ! grade_home "$pass" pass-fixture >/dev/null; then echo "SELF-TEST BUG: pass fixture graded FAIL" >&2; rc=1; fi
  # Missing DB must ERROR (exit 2), not FAIL.
  mkdir -p "$tmp/empty/.newt"
  grade_home "$tmp/empty" empty-fixture >/dev/null; [ $? -eq 2 ] || { echo "SELF-TEST BUG: empty home did not ERROR" >&2; rc=1; }
  if [ $rc -eq 0 ]; then echo "SELF-TEST: OK" >&2; else echo "SELF-TEST: FAILED" >&2; fi
  return $rc
}

# ---- arg parsing ----
main() {
  if [ "${1:-}" = "--self-test" ]; then self_test; exit $?; fi

  local newt="${1:?usage: grade-loop.sh <newt-binary> --home <dir> [--prompts f] [--workdir d] [--label s] [--timeout s]  |  grade-loop.sh --self-test}"
  shift
  local home="" prompts="" workdir="$REPO" label="loop" timeout_s=1200
  while [ $# -gt 0 ]; do
    case "$1" in
      --home) home="$2"; shift 2;;
      --prompts) prompts="$2"; shift 2;;
      --workdir) workdir="$2"; shift 2;;
      --label) label="$2"; shift 2;;
      --timeout) timeout_s="$2"; shift 2;;
      *) echo "grade-loop: unknown arg '$1'" >&2; exit 2;;
    esac
  done
  [ -n "$home" ] || { echo "grade-loop: --home <throwaway-with-.newt> is required" >&2; exit 2; }
  if [ -z "$prompts" ]; then prompts="$(mktemp)"; default_prompts >"$prompts"; fi

  run_yardstick "$newt" "$home" "$prompts" "$workdir" "$timeout_s" || exit 2
  grade_home "$home" "$label"
}

main "$@"
