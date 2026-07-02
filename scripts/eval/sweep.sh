#!/usr/bin/env bash
# sweep.sh — multi-trial matrix driver over ratchet.sh (issue #804).
#
# ratchet.sh runs ONE (task × mode × model) cell per invocation; this driver
# runs the full matrix with n trials per cell, durable append-only results,
# and crash resume: re-running the identical command tops up only the trials
# that are missing. Nothing here replaces ratchet.sh — it stays the one-cell
# primitive (nightly-eval.yml and file-regressions.sh depend on its contract).
#
# Why this exists (the #802/#803 lesson): every DGX sweep cell was n=1 and
# the headline "30b × T2 FAIL" turned out to be noise (~80% PASS at n=5).
# Per-cell claims need n>=5; this is the instrument that makes that routine.
#
# HONEST TRIALS: a row counts toward n only if the model was actually
# exercised. Rows that are really infrastructure failures wearing a FAIL
# label — crew "no_crew_branch_infra" (dead endpoint / model not pulled;
# ratchet.sh discriminates from the plan log, #820) or single-mode output
# with an EMPTY tests_pass (the evaluator never ran) — are logged to
# errors.log and retried on resume, never appended. A crew run where real
# inference happened but nothing landed ("no_crew_branch_exercised") is a
# LEGITIMATE behavioral FAIL and counts. If a model group's FIRST contact
# is an infra failure the whole group is skipped this invocation
# (dead-endpoint canary): a 20-hour sweep must not fill with well-formed
# connection noise and report DONE.
#
# MODEL/ENDPOINT PAIRING: in crew mode ratchet.sh's --model is label-only —
# what actually runs comes from $NEWT_CONFIG (newt plan honors it). In single
# mode the ACP worker IGNORES config [[backends]] of kind="ollama" and
# discovers its endpoint, honoring $OLLAMA_HOST verbatim. So per swept model
# this driver (a) generates an ephemeral $NEWT_CONFIG from a {{MODEL}}
# template and asserts the substituted, uncommented `model = "..."` line, and
# (b) exports $OLLAMA_HOST from that config's endpoint — one variable feeds
# label, crew config, and single-mode endpoint alike.
#
# SECURITY (RATCHET.md invariant): no home-network specifics land in anything
# this script writes under results/. Models are NAMES; endpoints live only in
# the operator's LOCAL template (default ~/.newt/eval-sweeps/template.toml,
# shape in RATCHET.local.example) and are exported as runtime env, never
# recorded. The throwaway root is refused under /home so dir= fields in the
# TSV cannot leak usernames.
#
# Usage:
#   scripts/eval/sweep.sh --out scripts/eval/results/sweeps/<name> \
#       --tasks T2-humanize-duration,010-decompose-god-function \
#       --modes single,crew \
#       --models qwen2.5-coder:14b,qwen3-coder:30b --trials 5 [--coder]
#   scripts/eval/sweep.sh --out <dir> --status     # completion grid; no runs
#   scripts/eval/sweep.sh --out <dir> --reap       # rm the throwaway tree
#   scripts/eval/sweep.sh --self-test              # offline checks, no binaries
#
# Detached launch (survives logout/session death; requires loginctl linger):
#   systemd-run --user --unit newt-sweep-<name> \
#     --working-directory "$PWD" \
#     --setenv NEWT_BIN="$PWD/target/release/newt" \
#     --setenv NEWT_EVAL_BIN="$PWD/target/release/newt-eval" \
#     scripts/eval/sweep.sh --out ... --tasks ... --modes ... --models ... --trials 5
#   journalctl --user -fu newt-sweep-<name>   # logs
#   scripts/eval/sweep.sh --out <dir> --status # progress
#
# Results contract (consumed by the .claude/workflows analysis suite):
#   $OUT/sweep.tsv       append-only; one row per COMPLETED, MODEL-EXERCISED
#                        trial. Cols 1-6 are ratchet.sh's exact format
#                        (file-regressions.sh reads $1,$2,$3,$5 — extra cols
#                        are backward-compatible); col 7 = ISO-8601 UTC
#                        completion time; col 8 = duration seconds; col 9 =
#                        run parameters (max_leaves=..;timeout=..;sha=..) so
#                        parameter drift across resumes is visible per row.
#   $OUT/sweep.grid      target grid: task<TAB>mode<TAB>model<TAB>trials.
#                        The grid is the completion AUTHORITY (raise-never-
#                        lower); to shrink a target, edit this file.
#   $OUT/sweep.meta.json first-invocation context (git sha, grid, sanitized).
#   $OUT/errors.log      infra failures — retried on the next resume.
#   $OUT/logs/           per-cell stderr.
#   $OUT/.tmproot        absolute path of this sweep's throwaway tree.
#   $OUT/DONE            exists iff every grid cell has >= its target rows.
# Exit code: 0 = grid complete; 2 = incomplete (systemd shows failed => a
# resume is needed); other non-zero = refused to start.
# Crash caveat: a PASS throwaway whose reap was interrupted survives until
# --reap; the 14-day /var/tmp/newt-sweeps policy in RATCHET.md is the backstop.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
RATCHET="$HERE/ratchet.sh"
TMP_ROOT="${SWEEP_TMP_ROOT:-/var/tmp/newt-sweeps}"

OUT="" TASKS="" MODES="" MODELS="" TRIALS=5 MAX_LEAVES=6 TIMEOUT=1200
WORKER_TIMEOUT_MS=120000 CODER="" KEEP="fail" TEMPLATE="${SWEEP_CONFIG_TEMPLATE:-$HOME/.newt/eval-sweeps/template.toml}"
ACTION="run"
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2;;
    --tasks) TASKS="$2"; shift 2;;
    --modes) MODES="$2"; shift 2;;
    --models) MODELS="$2"; shift 2;;
    --trials) TRIALS="$2"; shift 2;;
    --max-leaves) MAX_LEAVES="$2"; shift 2;;
    --timeout) TIMEOUT="$2"; shift 2;;
    --worker-timeout-ms) WORKER_TIMEOUT_MS="$2"; shift 2;;
    --coder) CODER="--coder"; shift;;
    --keep) KEEP="$2"; shift 2;;
    --config-template) TEMPLATE="$2"; shift 2;;
    --status) ACTION="status"; shift;;
    --reap) ACTION="reap"; shift;;
    --self-test) ACTION="self-test"; shift;;
    *) echo "sweep: unknown arg '$1'" >&2; exit 2;;
  esac
done

die() { echo "sweep: $*" >&2; exit 1; }
now_utc() { date -u +%FT%TZ; }
slug() { printf '%s' "$1" | tr ':/ .' '____'; }

# ---------------------------------------------------------------- pure logic
# (kept dependency-free and argument-driven so --self-test can exercise them)

# Rows already completed for one cell in a sweep.tsv. Torn/short rows (a
# crash mid-append) and rows without a PASS/FAIL grade never count.
count_done() { # tsv task mode model
  [ -f "$1" ] || { echo 0; return; }
  awk -F'\t' -v t="$2" -v mo="$3" -v m="$4" \
    'NF>=6 && $1=="RATCHET" && $2==t && $3==mo && $4==m && $5 ~ /^(PASS|FAIL)/ {n++} END {print n+0}' "$1"
}

# Structural row check: RATCHET-prefixed, model column matches the label we
# asked for. NOTE: ratchet echoes --model back, so this catches parse garbage
# and truncation, NOT config drift — the real model/endpoint pairing is
# enforced at config-generation time (see run_sweep) and by the canary.
validate_row() { # row model
  case "$1" in RATCHET"$(printf '\t')"*) :;; *) return 1;; esac
  [ "$(printf '%s' "$1" | awk -F'\t' '{print $4}')" = "$2" ]
}

# Infra classification: a row whose FAIL is really "the model was never
# exercised". crew: ratchet's no_crew_branch_infra marker (or the legacy
# bare no_crew_branch, kept conservative); no_crew_branch_exercised means
# real inference happened and the crew landed nothing — a LEGITIMATE
# behavioral FAIL that counts toward n (#820). single: the evaluator
# emitted no tests_pass value at all.
row_is_infra() { # row
  local mode details tp
  mode="$(printf '%s' "$1" | awk -F'\t' '{print $3}')"
  details="$(printf '%s' "$1" | awk -F'\t' '{print $6}')"
  case "$mode" in
    crew)
      case "$details" in
        *no_crew_branch_exercised*) return 1;;
        *no_crew_branch*) return 0;;
      esac;;
    single)
      tp="$(printf '%s' "$details" | sed -n 's/.*tests_pass=\([^ ]*\).*/\1/p')"
      [ -z "$tp" ] && return 0;;
  esac
  return 1
}

# True (exit 0) iff every grid cell has reached its trial target.
grid_complete() { # grid tsv
  local task mode model want got
  while IFS=$'\t' read -r task mode model want; do
    [ -n "$task" ] || continue
    got="$(count_done "$2" "$task" "$mode" "$model")"
    [ "$got" -ge "$want" ] || return 1
  done < "$1"
  return 0
}

# The grid's recorded target for one cell (the grid, not argv, is authority).
grid_target() { # grid task mode model
  awk -F'\t' -v t="$2" -v mo="$3" -v m="$4" \
    '$1==t && $2==mo && $3==m {print $4; exit}' "$1"
}

# Guarded throwaway removal: only ever delete real directories that resolve
# under the sweep tmp root (no symlinks, no prefix tricks on /var/tmp).
reap_dir() { # dir
  local real root
  [ -e "$1" ] || return 0
  [ -L "$1" ] && { echo "sweep: refusing to reap symlink '$1'" >&2; return 1; }
  real="$(realpath -m "$1" 2>/dev/null)" || return 1
  root="$(realpath -m "$TMP_ROOT" 2>/dev/null)" || return 1
  case "$real" in
    "$root"/*) [ -d "$real" ] && rm -rf "$real";;
    *) echo "sweep: refusing to reap '$1' (outside $TMP_ROOT)" >&2; return 1;;
  esac
}

# Merge (task,mode,model,trials) targets into the grid file: add missing
# cells; raise (never lower) an existing cell's trial target.
grid_merge() { # grid task mode model trials
  local tmp
  tmp="$(mktemp)"
  if [ -f "$1" ]; then cp "$1" "$tmp"; fi
  awk -F'\t' -v OFS='\t' -v t="$2" -v mo="$3" -v m="$4" -v n="$5" '
    $1==t && $2==mo && $3==m { seen=1; if ($4<n) $4=n }
    { print }
    END { if (!seen) print t, mo, m, n }
  ' "$tmp" > "$1.new" && mv "$1.new" "$1"
  rm -f "$tmp"
}

# Repair a torn final line (crash/ENOSPC mid-append): give the fragment its
# own newline so the next append cannot merge into it. The fragment itself
# stays un-counted (count_done's shape filter).
repair_tsv_tail() { # tsv
  [ -s "$1" ] || return 0
  [ -n "$(tail -c1 "$1")" ] && echo >> "$1"
  return 0
}

status_report() { # grid tsv done_marker
  local task mode model want got total_want=0 total_got=0
  printf '%-32s %-7s %-24s %s\n' TASK MODE MODEL DONE/TARGET
  while IFS=$'\t' read -r task mode model want; do
    [ -n "$task" ] || continue
    got="$(count_done "$2" "$task" "$mode" "$model")"
    total_want=$((total_want + want)); total_got=$((total_got + (got>want ? want : got)))
    printf '%-32s %-7s %-24s %s/%s\n' "$task" "$mode" "$model" "$got" "$want"
  done < "$1"
  echo "cells: $total_got/$total_want trials complete"
  if [ -f "$2" ]; then
    echo "last row: $(awk -F'\t' 'END{print $7" ("$2"/"$3"/"$4" -> "$5")"}' "$2")"
    awk -F'\t' '$1=="RATCHET" && $8 ~ /^[0-9]+$/ {s+=$8; n++} END {if (n) printf "mean trial duration: %ds over %d rows\n", s/n, n}' "$2"
  fi
  [ -e "$3" ] && echo "DONE ($(cat "$3"))" || echo "not done"
}

# ------------------------------------------------------------------ actions

# The single-quoted conditions in here are DEFERRED expressions eval'd by t()
# — SC2016 is the point, not a bug; the named locals are used inside them.
# shellcheck disable=SC2016,SC2034
self_test() {
  local sb; sb="$(mktemp -d)"
  local grid="$sb/sweep.grid" tsv="$sb/sweep.tsv" fails=0
  t() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails+1)); fi; }

  grid_merge "$grid" T2 crew m1 2
  grid_merge "$grid" T2 crew m2 2
  grid_merge "$grid" T2 crew m1 5   # raise, never lower
  grid_merge "$grid" T2 crew m1 3
  t "grid_merge dedups + keeps max trials" \
    '[ "$(wc -l < "$grid")" = 2 ] && grep -qP "T2\tcrew\tm1\t5" "$grid"'
  t "grid_target reads the recorded authority" '[ "$(grid_target "$grid" T2 crew m1)" = 5 ]'

  printf 'RATCHET\tT2\tcrew\tm1\tPASS\tleaves=1\t2026-01-01T00:00:00Z\t100\n' >> "$tsv"
  printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tleaves=3\t2026-01-01T00:10:00Z\t200\n' >> "$tsv"
  t "count_done counts PASS and FAIL rows"   '[ "$(count_done "$tsv" T2 crew m1)" = 2 ]'
  t "count_done is cell-scoped"              '[ "$(count_done "$tsv" T2 crew m2)" = 0 ]'
  t "count_done tolerates a missing tsv"     '[ "$(count_done "$sb/none.tsv" T2 crew m1)" = 0 ]'

  printf 'RATCHET\tT2\tcrew\tm1' >> "$tsv"        # torn row, no newline
  t "torn row does not count"                '[ "$(count_done "$tsv" T2 crew m1)" = 2 ]'
  repair_tsv_tail "$tsv"
  printf 'RATCHET\tT2\tcrew\tm1\tPASS\tx\tts\t1\n' >> "$tsv"
  t "repair_tsv_tail isolates the fragment"  '[ "$(count_done "$tsv" T2 crew m1)" = 3 ]'
  repair_tsv_tail "$tsv"
  t "repair_tsv_tail is idempotent on clean tails" '[ "$(count_done "$tsv" T2 crew m1)" = 3 ]'

  local good bad
  good="$(printf 'RATCHET\tT2\tcrew\tm1\tPASS\tx')"
  bad="$(printf 'RATCHET\tT2\tcrew\tOTHER\tPASS\tx')"
  t "validate_row accepts a matching row"    'validate_row "$good" m1'
  t "validate_row rejects a mislabeled row"  '! validate_row "$bad" m1'
  t "validate_row rejects a non-row"         '! validate_row "timed out" m1'

  local infra_crew infra_single beh_single beh_crew infra_marked exercised
  infra_crew="$(printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tplan_rc=1 no_crew_branch (see x)')"
  infra_marked="$(printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tplan_rc=1 no_crew_branch_infra (see x)')"
  exercised="$(printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tplan_rc=1 no_crew_branch_exercised dir=/x')"
  infra_single="$(printf 'RATCHET\tT2\tsingle\tm1\tFAIL\ttests_pass= all_evaluators_ok=no')"
  beh_single="$(printf 'RATCHET\tT2\tsingle\tm1\tFAIL\ttests_pass=fail all_evaluators_ok=no')"
  beh_crew="$(printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tleaves=3 plan_rc=1 dir=/x')"
  t "row_is_infra: legacy bare no_crew_branch"        'row_is_infra "$infra_crew"'
  t "row_is_infra: marked no_crew_branch_infra"       'row_is_infra "$infra_marked"'
  t "row_is_infra: EXERCISED no-land counts as trial" '! row_is_infra "$exercised"'
  t "row_is_infra: single empty tests_pass"  'row_is_infra "$infra_single"'
  t "row_is_infra: real single FAIL counts"  '! row_is_infra "$beh_single"'
  t "row_is_infra: real crew FAIL counts"    '! row_is_infra "$beh_crew"'

  t "grid_complete false while trials missing" '! grid_complete "$grid" "$tsv"'
  for _ in 1 2; do printf 'RATCHET\tT2\tcrew\tm1\tFAIL\tx\tts\t1\n' >> "$tsv"; done
  for _ in 1 2; do printf 'RATCHET\tT2\tcrew\tm2\tPASS\tx\tts\t1\n' >> "$tsv"; done
  t "grid_complete true when every cell reaches target" 'grid_complete "$grid" "$tsv"'

  mkdir -p "$TMP_ROOT/self-test/x" 2>/dev/null
  t "reap_dir removes inside the tmp root"   'reap_dir "$TMP_ROOT/self-test/x" && [ ! -d "$TMP_ROOT/self-test/x" ]'
  t "reap_dir refuses outside the tmp root"  '! reap_dir "$sb" 2>/dev/null && [ -d "$sb" ]'
  mkdir -p "$TMP_ROOT/self-test"
  ln -s "$sb" "$TMP_ROOT/self-test/link"
  t "reap_dir refuses a symlink"             '! reap_dir "$TMP_ROOT/self-test/link" 2>/dev/null && [ -d "$sb" ]'

  rm -rf "$sb" "$TMP_ROOT/self-test"
  if [ "$fails" = 0 ]; then echo "self-test: all ok"; else die "self-test: $fails failure(s)"; fi
}

preflight() {
  if [ -z "$TASKS" ] || [ -z "$MODES" ] || [ -z "$MODELS" ]; then
    die "--tasks, --modes, --models are required"
  fi
  case "$OUT" in *[[:space:]]*) die "--out must not contain whitespace (dir= parsing, reaping)";; esac
  case "$(realpath -m "$TMP_ROOT")" in
    /home/*) die "throwaway root under /home would leak usernames into committable TSVs (dir= fields); use /var/tmp";;
  esac
  [ -x "$RATCHET" ] || die "ratchet.sh not executable at $RATCHET"
  [ -f "$TEMPLATE" ] || die "config template not found: $TEMPLATE (see RATCHET.local.example)"
  grep -qE '^[[:space:]]*model[[:space:]]*=[[:space:]]*"\{\{MODEL\}\}"' "$TEMPLATE" \
    || die "config template needs an UNCOMMENTED 'model = \"{{MODEL}}\"' line: $TEMPLATE"
  grep -qE '^[[:space:]]*endpoint[[:space:]]*=' "$TEMPLATE" \
    || die "config template needs an endpoint line (single mode is steered via OLLAMA_HOST): $TEMPLATE"
  NEWT="${NEWT_BIN:-$REPO/target/release/newt}"
  NEWT_EVAL="${NEWT_EVAL_BIN:-$REPO/target/release/newt-eval}"
  [ -x "$NEWT" ] || die "newt binary not at $NEWT — build release first (never rebuild mid-sweep)"
  [ -x "$NEWT_EVAL" ] || die "newt-eval binary not at $NEWT_EVAL — build release first"
  export NEWT_BIN="$NEWT" NEWT_EVAL_BIN="$NEWT_EVAL"
  for task in ${TASKS//,/ }; do
    [ -f "$REPO/newt-eval/cases/$task/case.toml" ] || die "no case at newt-eval/cases/$task"
  done
  for mode in ${MODES//,/ }; do
    case "$mode" in single|crew) :;; *) die "--modes entries must be single|crew (got '$mode')";; esac
  done
  case "$KEEP" in fail|all|none) :;; *) die "--keep must be fail|all|none";; esac
}

write_meta() { # meta_path
  # Sanitized: this file lands in committable results/. Strip $HOME anywhere;
  # if a username-bearing path survives, record the basename only.
  local tpl="${TEMPLATE//$HOME/\~}"
  case "$tpl" in *"/home/"*) tpl="(local)/$(basename "$TEMPLATE")";; esac
  cat > "$1" <<EOF
{
  "name": "$(basename "$OUT")",
  "created": "$(now_utc)",
  "git_sha": "$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)",
  "tasks": "$TASKS",
  "modes": "$MODES",
  "models": "$MODELS",
  "trials": $TRIALS,
  "max_leaves": $MAX_LEAVES,
  "timeout": $TIMEOUT,
  "keep": "$KEEP",
  "config_template": "$tpl"
}
EOF
}

run_sweep() {
  preflight
  mkdir -p "$OUT/logs" || die "cannot create $OUT"
  OUT="$(realpath "$OUT")"   # one identity regardless of invoking cwd
  exec 9>"$OUT/.lock"
  flock -n 9 || die "another sweep is writing to $OUT (lock held)"

  local grid="$OUT/sweep.grid" tsv="$OUT/sweep.tsv" errs="$OUT/errors.log" done_f="$OUT/DONE"
  touch "$tsv" || die "cannot write $tsv"
  repair_tsv_tail "$tsv"
  for model in ${MODELS//,/ }; do for task in ${TASKS//,/ }; do for mode in ${MODES//,/ }; do
    grid_merge "$grid" "$task" "$mode" "$model" "$TRIALS"
  done; done; done
  [ -f "$OUT/sweep.meta.json" ] || write_meta "$OUT/sweep.meta.json"
  # A widened grid can invalidate a previous DONE.
  grid_complete "$grid" "$tsv" || rm -f "$done_f"

  # Throwaway tree: unique per absolute $OUT (basename alone collides across
  # sweeps with the same name — reaping one would destroy the other's).
  local sweep_tmp
  sweep_tmp="$TMP_ROOT/$(basename "$OUT")-$(printf '%s' "$OUT" | cksum | awk '{print $1}')"
  mkdir -p "$sweep_tmp" || die "cannot create throwaway root $sweep_tmp"
  chmod 0700 "$sweep_tmp" 2>/dev/null || true
  printf '%s\n' "$sweep_tmp" > "$OUT/.tmproot"
  export TMPDIR="$sweep_tmp"   # ratchet's mktemp -d lands here

  local git_sha
  # cfg_dir is deliberately GLOBAL: the EXIT trap fires at top-level scope
  # after run_sweep returns, where a `local` would be out of scope (set -u
  # would see it unset and the generated endpoint-bearing configs would leak
  # into the sweep tmp tree until --reap).
  cfg_dir="$(mktemp -d)"
  trap 'rm -rf "${cfg_dir:-}"' EXIT
  git_sha="$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)"

  # MODEL-MAJOR: one config/model load per group; tasks × modes × trials inner.
  for model in ${MODELS//,/ }; do
    local cfg endpoint model_contact=0 group_skip=0
    cfg="$cfg_dir/$(slug "$model").toml"
    sed "s|{{MODEL}}|$model|g" "$TEMPLATE" > "$cfg"
    grep -qE "^[[:space:]]*model[[:space:]]*=[[:space:]]*\"$(printf '%s' "$model" | sed 's/[.[\*^$]/\\&/g')\"" "$cfg" \
      || die "template substitution failed for $model"
    endpoint="$(sed -n 's/^[[:space:]]*endpoint[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$cfg" | head -1)"
    [ -n "$endpoint" ] || die "no endpoint in generated config for $model"
    # One variable feeds all three consumers of "which model/backend":
    export NEWT_CONFIG="$cfg"       # crew: newt plan resolves this
    export OLLAMA_HOST="$endpoint"  # single: the ACP worker honors this verbatim
    for task in ${TASKS//,/ }; do
      [ "$group_skip" = 1 ] && break
      for mode in ${MODES//,/ }; do
        [ "$group_skip" = 1 ] && break
        local want got
        want="$(grid_target "$grid" "$task" "$mode" "$model")"
        got="$(count_done "$tsv" "$task" "$mode" "$model")"
        while [ "$got" -lt "$want" ]; do
          local log
          log="$OUT/logs/$task.$mode.$(slug "$model").log"
          echo "[$(now_utc)] cell $task/$mode/$model trial $((got+1))/$want" | tee -a "$log" >&2
          local t0=$SECONDS raw row rc
          local extra=""
          [ "$mode" = "single" ] && extra="$CODER --worker-timeout-ms $WORKER_TIMEOUT_MS"
          # Outer timeout: ratchet's own --timeout bounds only `newt plan`;
          # its grading `cargo test` is unbounded. -k escalates to KILL for
          # TERM-immune children. fd 9 (the lock) is closed for the child so
          # an orphaned run can never hold the sweep lock.
          # shellcheck disable=SC2086
          raw="$(timeout -k 60 $((TIMEOUT + 900)) "$RATCHET" \
                  --task "$task" --mode "$mode" --model "$model" \
                  --max-leaves "$MAX_LEAVES" --timeout "$TIMEOUT" $extra \
                  2>>"$log" 9>&-)"
          rc=$?
          row="$(printf '%s\n' "$raw" | grep '^RATCHET' | tail -1 || true)"
          if validate_row "$row" "$model" && ! row_is_infra "$row"; then
            printf '%s\t%s\t%s\t%s\n' "$row" "$(now_utc)" "$((SECONDS - t0))" \
              "max_leaves=$MAX_LEAVES;timeout=$TIMEOUT;sha=$git_sha" >> "$tsv" \
              || die "cannot append to $tsv (disk full?)"
            got=$((got + 1)); model_contact=1
            local behavioral dir
            behavioral="$(printf '%s' "$row" | awk -F'\t' '{print $5}')"
            dir="$(printf '%s' "$row" | sed -n 's/.*dir=\([^ ]*\).*/\1/p')"
            if [ -n "$dir" ]; then
              case "$KEEP" in
                none) reap_dir "$dir" || true;;
                # PASS?gameable trees are kept: they are exactly the
                # possibly-gamed evidence autopsy needs.
                fail) [ "$behavioral" = "PASS" ] && { reap_dir "$dir" || true; };;
              esac
            fi
          else
            local why="rc=$rc row='${row:-none}'"
            row_is_infra "${row:-}" 2>/dev/null && why="infra-row: ${row:-}"
            echo "$(now_utc) INFRA-ERROR $task/$mode/$model $why (see logs/)" >> "$errs"
            if [ "$model_contact" = 0 ]; then
              # Dead-endpoint canary: first contact for this model failed —
              # do not grind the whole group into noise; skip it this run.
              echo "$(now_utc) MODEL-GROUP-SKIPPED $model (no successful contact; fix the backend and re-run)" >> "$errs"
              group_skip=1
            fi
            # Never tight-loop a broken cell: move on; resume retries it.
            break
          fi
        done
      done
    done
  done

  if grid_complete "$grid" "$tsv"; then
    now_utc > "$done_f"
    echo "sweep: complete — $done_f written" >&2
    status_report "$grid" "$tsv" "$done_f"
  else
    echo "sweep: incomplete (infra errors or interrupted) — re-run the same command to top up; see $errs" >&2
    status_report "$grid" "$tsv" "$done_f"
    exit 2
  fi
}

case "$ACTION" in
  self-test) self_test;;
  status)
    [ -n "$OUT" ] || die "--status needs --out"
    OUT="$(realpath -m "$OUT")"
    [ -f "$OUT/sweep.grid" ] || die "no sweep at $OUT (missing sweep.grid)"
    status_report "$OUT/sweep.grid" "$OUT/sweep.tsv" "$OUT/DONE";;
  reap)
    [ -n "$OUT" ] || die "--reap needs --out"
    OUT="$(realpath -m "$OUT")"
    [ -f "$OUT/.tmproot" ] || die "no throwaway tree recorded at $OUT/.tmproot"
    # Refuse to reap under a live sweep.
    exec 9>"$OUT/.lock"
    flock -n 9 || die "sweep at $OUT is running (lock held) — stop it before --reap"
    reap_dir "$(cat "$OUT/.tmproot")" && echo "sweep: reaped $(cat "$OUT/.tmproot")";;
  run) run_sweep;;
esac
