#!/usr/bin/env bash
# loop-sweep.sh — n>=5 A/B sweep for the loop-completion yardstick (#971).
#
# ratchet.sh/sweep.sh drive (task x mode x model) cells graded by cargo-test /
# newt-eval. This driver is their sibling for the DIFFERENT question the
# next-loop-levers work asks: run the verbatim yardstick prompts against a fixed
# model, flip ONE lever per cell, and grade each trial with grade-loop.sh (which
# reads the run's own conversations.db). n>=5 per cell, durable append-only,
# crash-resume — the #803 lesson (plausible != measured) applied to the loop.
#
#   cell            = one lever (baseline, T0.1, T0.2, or a swapped --newt binary)
#   trial           = one yardstick run, graded PASS/FAIL by grade-loop.sh
#   grade-mover     = a lever whose pass-rate beats baseline at n>=5 (the /ab-gate)
#
# SECURITY (RATCHET.md invariant): this script names no host. The backend (and
# any summarizer) endpoint lives ONLY in the operator's LOCAL template dir
# (default ~/.newt/eval-sweeps/, shape in loop-template.example). Nothing under
# --out records a host; the throwaway HOME root is refused under /home so no
# username leaks. Models are NAMES.
#
# Usage:
#   loop-sweep.sh --out scripts/eval/results/loop-sweeps/<name> --newt <bin> \
#       --levers baseline,T0.1,T0.2 --trials 5 [--model ornith:35b] \
#       [--template ~/.newt/eval-sweeps] [--scratch /var/tmp/loop-sweep] \
#       [--workdir <cloned-newt-agent>]
#   loop-sweep.sh --out <dir> --status     # completion grid + pass-rates; no runs
#   loop-sweep.sh --out <dir> --reap       # rm the throwaway HOME trees
#   loop-sweep.sh --self-test              # offline checks, no binaries/backend
#
# Detached (survives logout; needs `loginctl enable-linger`):
#   systemd-run --user --unit loop-sweep-<name> --working-directory "$PWD" \
#     scripts/eval/loop-sweep.sh --out ... --newt ... --levers ... --trials 5
#
# Code levers (L1..): rebuild newt with the lever and run a fresh sweep with
# --newt <that-binary> --levers baseline; compare its baseline cell against the
# feature-off binary's baseline cell (cross-binary A/B). Config levers (T0.1,
# T0.2) toggle within a single binary here.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GRADE="$HERE/grade-loop.sh"

OUT="" NEWT="" LEVERS="baseline" TRIALS=5 MODEL="ornith:35b"
TEMPLATE="${HOME}/.newt/eval-sweeps" SCRATCH="/var/tmp/loop-sweep"
WORKDIR="$(cd "$HERE/../.." && pwd)" MODE="run"
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2;;
    --newt) NEWT="$2"; shift 2;;
    --levers) LEVERS="$2"; shift 2;;
    --trials) TRIALS="$2"; shift 2;;
    --model) MODEL="$2"; shift 2;;
    --template) TEMPLATE="$2"; shift 2;;
    --scratch) SCRATCH="$2"; shift 2;;
    --workdir) WORKDIR="$2"; shift 2;;
    --status) MODE="status"; shift;;
    --reap) MODE="reap"; shift;;
    --self-test) MODE="self-test"; shift;;
    *) echo "loop-sweep: unknown arg '$1'" >&2; exit 2;;
  esac
done

# --- render one lever's isolated HOME from the operator's local template ---
# Endpoint lives only in $TEMPLATE/loop-backend.toml.tmpl ({{MODEL}} +
# {{MAX_ROUNDS}} placeholders, plus the operator's [tui.permissions] choice —
# it MUST grant non-interactive access or the pipe drive stalls). T0.2 also
# drops in $TEMPLATE/loop-summarizer.toml; baseline/T0.1 leave it absent.
render_home() { # $1=lever $2=model $3=template_dir $4=home
  local lever="$1" model="$2" tdir="$3" home="$4" rounds=25
  [ "$lever" = "T0.1" ] && rounds=40
  local tmpl="$tdir/loop-backend.toml.tmpl"
  [ -f "$tmpl" ] || { echo "loop-sweep: missing $tmpl (copy scripts/eval/loop-template.example there)" >&2; return 2; }
  mkdir -p "$home/.newt"
  sed -e "s|{{MODEL}}|$model|g" -e "s|{{MAX_ROUNDS}}|$rounds|g" "$tmpl" >"$home/.newt/config.toml"
  if [ "$lever" = "T0.2" ]; then
    if [ ! -f "$tdir/loop-summarizer.toml" ]; then
      echo "loop-sweep: T0.2 needs $tdir/loop-summarizer.toml (off-box summarizer)" >&2; return 2
    fi
    cp "$tdir/loop-summarizer.toml" "$home/.newt/summarizer.toml"
  else
    rm -f "$home/.newt/summarizer.toml"
  fi
}

# A throwaway HOME root under /home would leak the username into dir paths.
scratch_safe() { case "$1" in /home/*|"$HOME"/*) return 1;; *) return 0;; esac; }

# --- pass-rate over a results file (one JSON line per trial) ---
# NB: `grep -c` prints 0 AND exits 1 on no-match — so NO `|| echo 0` (that would
# double-count to "0\n0"); an empty capture (missing file) defaults via ${:-0}.
pass_rate() { # $1=jsonl -> "pass/total"
  local f="$1" total pass
  [ -f "$f" ] || { echo "0/0"; return; }
  total="$(grep -c '"pass":' "$f" 2>/dev/null)"; total="${total:-0}"
  pass="$(grep -c '"pass":true' "$f" 2>/dev/null)"; pass="${pass:-0}"
  echo "$pass/$total"
}

# --- /ab-gate verdict: a lever moves the grade only at n>=5 and pass-rate up ---
gate() { # $1=base_jsonl $2=lever_jsonl -> verdict string
  local br lr bp bt lp lt
  br="$(pass_rate "$1")"; lr="$(pass_rate "$2")"
  bp="${br%/*}"; bt="${br#*/}"; lp="${lr%/*}"; lt="${lr#*/}"
  if [ "$lt" -lt 5 ]; then echo "INCONCLUSIVE (n=$lt<5): $lr"; return; fi
  if [ "$bt" -lt 5 ]; then echo "NO BASELINE (n=$bt<5)"; return; fi
  # integer percent, avoid bc dependency
  local bpc lpc; bpc=$(( bp*100/bt )); lpc=$(( lp*100/lt ))
  if [ "$lpc" -gt "$bpc" ]; then echo "MOVED +$(( lpc-bpc ))pp ($lr vs base $br)";
  else echo "no move ($lr vs base $br)"; fi
}

status() {
  [ -d "$OUT" ] || { echo "no results at $OUT" >&2; exit 1; }
  echo "lever          pass-rate   vs-baseline"
  echo "-------------- ----------- ------------------------"
  local base="$OUT/baseline.jsonl"
  for f in "$OUT"/*.jsonl; do
    [ -f "$f" ] || continue
    local name; name="$(basename "$f" .jsonl)"
    if [ "$name" = "baseline" ]; then printf '%-14s %-11s %s\n' "$name" "$(pass_rate "$f")" "(reference)";
    else printf '%-14s %-11s %s\n' "$name" "$(pass_rate "$f")" "$(gate "$base" "$f")"; fi
  done
}

reap() { rm -rf "${SCRATCH:?}/"* 2>/dev/null; echo "reaped throwaway HOMEs under $SCRATCH" >&2; }

run() {
  [ -n "$OUT" ]  || { echo "loop-sweep: --out required" >&2; exit 2; }
  [ -n "$NEWT" ] || { echo "loop-sweep: --newt <binary> required" >&2; exit 2; }
  [ -x "$GRADE" ] || { echo "loop-sweep: grade-loop.sh not executable at $GRADE" >&2; exit 2; }
  scratch_safe "$SCRATCH" || { echo "loop-sweep: refuse throwaway root under /home (leaks usernames); use --scratch /var/tmp/..." >&2; exit 2; }
  mkdir -p "$OUT" "$SCRATCH"
  IFS=',' read -r -a levers <<<"$LEVERS"
  for lever in "${levers[@]}"; do
    local jf="$OUT/$lever.jsonl" have need
    have="$(grep -c '"pass":' "$jf" 2>/dev/null)"; have="${have:-0}"
    need=$(( TRIALS - have ))
    [ "$need" -le 0 ] && { echo "== $lever: complete ($have/$TRIALS) ==" >&2; continue; }
    echo "== $lever: $have/$TRIALS done, running $need more ==" >&2
    local i
    for (( i=0; i<need; i++ )); do
      local home; home="$(mktemp -d "$SCRATCH/${lever}.XXXXXX")"
      if ! render_home "$lever" "$MODEL" "$TEMPLATE" "$home"; then rm -rf "$home"; exit 2; fi
      # A trial that errors (no persistence) is logged to errors.log and retried
      # on resume — never appended as a FAIL (honest trials only, per sweep.sh).
      if "$GRADE" "$NEWT" --home "$home" --workdir "$WORKDIR" --label "$lever" \
            >>"$jf.tmp" 2>>"$OUT/$lever.stderr.log"; then :; fi
      if tail -1 "$jf.tmp" | grep -q '"error"'; then
        tail -1 "$jf.tmp" >>"$OUT/errors.log"; echo "  trial errored (see errors.log); will retry on resume" >&2
      else
        tail -1 "$jf.tmp" >>"$jf"
      fi
      rm -f "$jf.tmp"; rm -rf "$home"
    done
  done
  echo >&2; status
}

# --- offline self-test: pure logic only (rendering, pass-rate, gate, guard) ---
self_test() {
  local tmp rc=0; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' RETURN
  # 1. render substitutes placeholders and toggles the summarizer.
  mkdir -p "$tmp/tmpl"
  printf '[[backends]]\nendpoint="http://example:11434"\nmodel="{{MODEL}}"\n[tui]\nmax_tool_rounds={{MAX_ROUNDS}}\n' >"$tmp/tmpl/loop-backend.toml.tmpl"
  printf 'kind="ollama"\nmodel="gemma3:12b"\n' >"$tmp/tmpl/loop-summarizer.toml"
  render_home baseline ornith:35b "$tmp/tmpl" "$tmp/hb"
  grep -q 'model="ornith:35b"' "$tmp/hb/.newt/config.toml"      || { echo "ST: model not substituted" >&2; rc=1; }
  grep -q 'max_tool_rounds=25' "$tmp/hb/.newt/config.toml"      || { echo "ST: baseline rounds!=25" >&2; rc=1; }
  [ -f "$tmp/hb/.newt/summarizer.toml" ]                        && { echo "ST: baseline should have no summarizer.toml" >&2; rc=1; }
  render_home T0.1 ornith:35b "$tmp/tmpl" "$tmp/h1"
  grep -q 'max_tool_rounds=40' "$tmp/h1/.newt/config.toml"      || { echo "ST: T0.1 rounds!=40" >&2; rc=1; }
  render_home T0.2 ornith:35b "$tmp/tmpl" "$tmp/h2"
  [ -f "$tmp/h2/.newt/summarizer.toml" ]                        || { echo "ST: T0.2 missing summarizer.toml" >&2; rc=1; }
  # 2. pass_rate + gate math.
  printf '{"pass":true}\n{"pass":true}\n{"pass":false}\n' >"$tmp/base.jsonl"
  printf '{"pass":true}\n{"pass":true}\n{"pass":true}\n{"pass":true}\n{"pass":true}\n' >"$tmp/lev.jsonl"
  [ "$(pass_rate "$tmp/base.jsonl")" = "2/3" ] || { echo "ST: pass_rate wrong" >&2; rc=1; }
  printf '{"pass":true}\n{"pass":false}\n{"pass":false}\n{"pass":false}\n{"pass":false}\n' >"$tmp/base5.jsonl"
  case "$(gate "$tmp/base5.jsonl" "$tmp/lev.jsonl")" in
    MOVED*) : ;; *) echo "ST: gate should report MOVED (5/5 vs 1/5)" >&2; rc=1;;
  esac
  case "$(gate "$tmp/base5.jsonl" "$tmp/base.jsonl")" in
    INCONCLUSIVE*) : ;; *) echo "ST: gate should be INCONCLUSIVE for n<5 lever" >&2; rc=1;;
  esac
  # 3. security guard rejects /home scratch, allows /var/tmp.
  scratch_safe "$HOME/x"    && { echo "ST: guard let /home through" >&2; rc=1; }
  scratch_safe "/var/tmp/x" || { echo "ST: guard rejected /var/tmp" >&2; rc=1; }
  [ $rc -eq 0 ] && echo "SELF-TEST: OK" >&2 || echo "SELF-TEST: FAILED" >&2
  return $rc
}

case "$MODE" in
  status) status;;
  reap) reap;;
  self-test) self_test;;
  run) run;;
esac
