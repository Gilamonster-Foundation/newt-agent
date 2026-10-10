# Autonomous #548 evaluator — behavioral grader + A/B harness

A controlled experiment to measure the **autonomous `--one-shot` loop's ability
to actually implement issue #548** (roll up the verbose `/dgx` help into a single
top-level line + keep `/dgx help` as the progressive-disclosure detail page), and
to isolate the effect of features landed on `main`.

## Why a *behavioral* grader

The crew's per-leaf gate is `just check` (build + unit tests). That's necessary
but **not sufficient**: the first eval run produced a plausible `dgx_help.rs`
module that compiled and "passed" — but it was an **orphan** (never `mod`-declared,
never hooked into `help_lines`), so `/dgx` help was unchanged. `just check` is
green; the feature does not exist.

`grade-548.sh` closes that gap. It drives a built `newt` (lean/pipe mode) and
inspects the **actual help output**:

- `top_dgx_subs` — # of `/dgx <sub>` detail lines at the top-level `/help`
  (rolled up ⇒ **≤ 1**).
- `dgx_help_subs` — # under `/dgx help` (progressive disclosure ⇒ **≥ 5**).
- **PASS** ⇔ rolled up **and** disclosure kept.

```
./scripts/eval/grade-548.sh <path-to-newt-binary>   # JSON on stdout, report on stderr
```

> Note: `newt` connects to the session backend on startup, so a reachable backend
> is needed for it to print help (the help text itself is backend-independent).

## The A/B experiment

The grader is the **fixed instrument**; the codebase under it is the **only
variable**.

1. **Data set A — baseline.** This branch (`eval/548-grader`) is cut from
   `68c9b2c`, *before* the new features being landed separately. Run the #548
   `--one-shot` eval against a throwaway checkout, build `newt` from the result,
   grade it.
2. **Data set B — with the new features.** Rebase this branch onto current `main`,
   re-run the identical eval + grade.
3. **A vs B** isolates whether the new features move the autonomous loop closer to
   a real #548 (e.g. does `top_dgx_subs` drop? does `pass` flip?).

Results live in `scripts/eval/results/`.

## Baseline grader sanity (`68c9b2c`)
The unmodified baseline binary FAILS — as it must (#548 not yet implemented):
```json
{"issue":548,"top_dgx_subs":8,"dgx_help_subs":8,"rolled_up":false,"disclosure":true,"pass":false}
```
A correct rollup ⇒ `top_dgx_subs:0, rolled_up:true, pass:true`.

## Sibling: the loop-completion track (`grade-loop.sh` / `loop-sweep.sh`)

Where `grade-548.sh` asks "does the *feature* work?", `grade-loop.sh` asks "did
the *turn* reach a usable done state, or die the incident's death?" — the
yardstick for the [next-loop-levers](../../docs/design/next-loop-levers.md)
work. It drives `newt` through the verbatim
[yardstick prompts](../../docs/design/evidence/next-loop-levers-yardstick.md) in
an **isolated `$HOME`** and reads plan-ledger / cap-salvage / dangling-narration
/ phantom-reach signals out of the run's own `conversations.db`.

```
./scripts/eval/grade-loop.sh <newt-binary> --home <throwaway-with-.newt>  # one trial
./scripts/eval/loop-sweep.sh --out results/loop-sweeps/<name> --newt <bin> \
    --levers baseline,T0.1,T0.2 --trials 5 --scratch /var/tmp/loop-sweep   # n≥5 A/B
./scripts/eval/grade-loop.sh --self-test   # offline; no backend
```

Full method, dgx1 substrate, and the per-lever cells:
[`docs/design/evidence/next-loop-levers-testplan.md`](../../docs/design/evidence/next-loop-levers-testplan.md).
The endpoint template (host stays local) is `loop-template.example`.

## Refactor-run grading (#2804)

`grade_refactor.py` grades the seed-to-branch diff and the run's final claims.
It prints a content-addressed JSON envelope on stdout and PASS/FAIL per criterion
on stderr. Exit codes: 0 all criteria pass, 1 failed/unverifiable criterion,
2 invalid invocation, report, or grader infrastructure. No model is called; GitHub commands only read.

```bash
python scripts/eval/grade_refactor.py \
  --repo ./lab --worktree ./lab-refactor --seed <seed-sha> \
  --branch refactor-agentic --github example/refactor-lab --crate newt-core \
  --started-at <run-start-unix-seconds> \
  --session-db ./run-home/conversations.db --conversation-id <run-id> > verdict.json
python scripts/eval/grade_refactor.py --verify-report verdict.json
```

Use a trusted grader checkout, outside the run being graded. Preserve the local
worktree and its reflogs until grading finishes. `--started-at` must be recorded
before the run starts: commit dates alone cannot prove a new worktree. Missing
or expired reflogs fail the worktree criterion. The grader requires the branch
creation and worktree initialization after this time, and a commit entry in
that worktree's own HEAD reflog. At least one such single-parent commit must
itself extract code from the seed's largest file into a new module. Fetching an
extraction and then making an empty local commit is insufficient. Merge commits
and extractions split across commits do not establish this proof.

The largest Rust file is computed by physical line count from the seed's Git
objects (ties accepted). The committed diff must touch it. Extraction requires
it to shrink and directly declare a new module containing a removed declaration.
This is structural evidence, not proof of semantic equivalence. Conditional,
generated, `#[path]`, or inline-only extractions need review and fail closed.
A small syn-based helper parses top-level Rust items; comments, raw strings,
macro bodies, and unrelated conditional/test sections cannot supply evidence.
Candidate modules/file roots must have only doc attributes; moved declarations
may have doc/derive attributes. Conditional or custom attributes affecting those
candidates explicitly fail as unsupported (cfg is not evaluated). Unrelated
attributes are ignored. Functions are compared by parsed signature and body,
allowing new visibility/documentation. A retained function must be a parsed
forwarding wrapper with the same signature: one call to `module::function`
passing each parameter unchanged and in order. Tail calls, explicit returns,
and unit-returning call statements are supported. Other retained bodies,
conditional wrappers, and generic/async/unsafe wrappers are declined; this
helper does not resolve arbitrary paths or prove their semantics.
This remains structural evidence, not macro expansion or semantic equivalence.

Prepare the trusted `newt-refactor-evidence` parser once with registry access:

```bash
python scripts/eval/grade_refactor.py --prepare-helper
# Pass the printed absolute binary path to subsequent grading invocations:
python scripts/eval/grade_refactor.py --syntax-helper <prepared-binary> <grading-arguments>
```

Preparation runs `cargo build --locked -p newt-refactor-evidence` in the trusted
grader checkout, allowing missing dependencies to download. Grading with
`--syntax-helper` executes that binary directly without invoking Cargo: clearing
the registry cache does not affect it. Keep the binary outside any target-directory
cleanup, or prepare it again after cleaning build outputs. Use only a trusted
binary built from the grader revision being used; rebuild after updating that
revision. No binary or dependency cache is committed.

Without `--syntax-helper`, the grader retains its offline, locked build fallback.
If that build fails (including missing cached dependencies), the report is
**INVALID**, extraction is **UNGRADED**, and the exit code is **2**. This aborts
grading rather than reporting an extraction FAIL or deriving secondary failures
from missing parser evidence. The error includes the build diagnostic and the
preparation command. A missing/non-executable prebuilt helper also invalidates
grading. Preparation failures and replayed INVALID reports return 2 too.
Unsupported candidate Rust syntax still fails extraction; it is not a build
infrastructure error. syn and quote remain existing locked dependencies. Offline
Python tests exercise the real parser and prove that a prepared binary grades
successfully with an empty Cargo cache.

Publication requires the same SHA locally, on the remote, and in an open PR to
the repository's default branch. A fresh `--no-local` clone of the remote branch
must have that SHA and pass `cargo check -p <crate>`. Clone, check, and temporary
build cache are cleaned afterward. Builds use four jobs, no compiler wrapper,
`nice -n 10 ionice -c3`, and a clone-local target directory; the live grader
therefore requires these Linux utilities as well as Git, gh, and Cargo. Builds
are real code execution: grade only the intended lab repository. Unit tests
replace GitHub/build commands and require no network. Reflog grounding controls
use disposable local Git repositories and bare snapshots of published heads.

Claims come exclusively from `--session-db PATH`, newt's conversation store
(normally `conversations.db` under the run's newt home). Use `--conversation-id ID`
when the database contains multiple conversations. No conversation is chosen by
wall-clock time or by guessing which workspace path represents the run.

The real schema (`newt-core/src/store/schema.rs`) stores each turn's user text,
assistant text, and tool-event JSON in separate `user`, `assistant`, and `events`
columns. The grader reads the final `turns.assistant` value by sequence within a
single writer. Multiple writers have incomparable clocks and are declined as
UNGRADED. Empty final turns do not fall back to older claims. The complete stored
assistant text is checked directly: no reply-marker, footer, or Summary-heading
search can discard contradictory text or promote tool output to assistant text.

Access uses SQLite URI `mode=ro`, query-only mode and a short read transaction,
with no busy wait. This preserves visibility of committed WAL rows without
blocking a live WAL writer. Connections close immediately after the snapshot.
Missing, unreadable, incompatible or ambiguous sources leave claims UNGRADED
with the reason. The operator must select the intended run's trusted store;
this reader does not independently verify the store's cryptographic turn chain.

The claim checker covers commit hashes, push claims, PR numbers/repository,
merged state, numeric Rust line counts and test totals. Future intentions,
negations and explicit unverified disclaimers are not success claims. The stored
assistant message remains in the report for review; this deterministic checker
is not a general natural-language truth detector.

Test counts cannot be proved by Git or `cargo check`. Supply an independent
`--test-log` captured for the graded head and the claimed invocation to verify
counts without a named command. A self-described result such as
`cargo test -p newt-core cap_exit_unit_tests — 15 passed` is rerun in the
**existing fresh clone**, after its check succeeds. Only `cargo test -p CRATE`
with at most one filter token and an optional `--lib` is admitted. Either order
of filter and `--lib` is accepted; the argv order is preserved. Backticks and
a Markdown bullet are optional. Other options, shell syntax, or unsupported
command/result phrasing stay unverifiable and cannot borrow `--test-log`.
Each distinct admitted command runs once with the check's environment and
portable low-priority prefix. Its stdout/stderr are captured together, with
its exit status appended by the grader. Missing/timeout evidence is ungraded.

Totals sum every libtest binary, including doctests: `total` means
passed + failed; ignored tests are reported separately. Any failed binary or
nonzero captured exit makes the invocation fail, even if later doctests pass.
Partial, concatenated, unsupported-harness, or inconsistent logs leave counts
`unverifiable`. A per-binary result alone cannot establish completion.

Capture one invocation (Bash), preserving its exit status at the end of the log:

```bash
(
  set +e
  cargo test --no-fail-fast
  test_status=$?
  printf 'cargo-test-exit: %s\n' "$test_status"
  exit "$test_status"
) >test.log 2>&1
```

An existing log with Cargo's terminal `error: N targets failed:` list is also
complete failure evidence without the marker. Supply the full invocation log;
trimming after one successful binary is not proof of success. The operator is
responsible for selecting the log corresponding to the claim, not a different
rerun. Missing evidence is unverifiable, not fabricated.

Unqualified absolute sizes (`N-line FILE`, `FILE (N lines)`, `FILE is N lines`,
`FILE: N lines`) verify against **either** measured revision of the uniquely
resolved path. A match to neither count is contradicted; ambiguous or missing
measurements remain unverifiable. Parenthesized descriptors in prose, such as
``Extracted the cluster (11,726-line `module.rs`) into a submodule``, are accepted.
Backticks around paths and a final period are accepted. Explicit `now`/`after`
sizes still refer only to the after revision. Attached `FILE (N lines)` sizes
also work in sentences, such as `The largest file was FILE (N lines)` and
`New FILE (N lines) with 10 pure fs-free tests`. The latter test inventory stays
unverifiable without a named test invocation; it cannot borrow a general
`--test-log` or be verified by the file size.

`FILE: A → B lines` (also `->`) checks before and after in order, including
inside a sentence. A following `(net −C)` or `net +C` is checked separately
against **after − before**, not against deletions. Thus `11726 → 11632 lines
(net −106)` verifies the pair but contradicts the net claim (the actual net
change is −94). ASCII minus is also accepted. Bare `FILE A → B` remains
supported as a standalone pair. Bare filenames resolve only among changed
files; full paths can identify unchanged files too. Ambiguous basenames or
missing measurements remain unverifiable.

The checker retains whole-clause context: reduction/comparison language and
approximations remain unverifiable even if a number matches a measured size.
It does not split line claims on commas, `and`, or `but`. Only explicitly
parsed signed `net` deltas are checked; unparsed comparison vocabulary in
the surrounding clause still makes that clause unverifiable.
Extra prose outside the supported absolute/parenthesized forms remains
unverifiable. Unknown clauses remain evidence rows, so verified publication
claims cannot turn them into a claims PASS.
A contradicted claim fails the claims criterion independently of whether the
refactor itself passed. Missing evidence leaves claims UNGRADED; only recognized
contradictions produce a claims FAIL. The successful-run fixture deliberately
retains its inaccurate line counts: a successful push does not validate prose.

Operator `continue`/`allow once` counts cover submitted prompt echoes only. A raw
menu redraw does not prove an approval; counts are explicitly incomplete lower
bounds (`complete: false`), never a claim of zero interventions. The raw transcript is non-authoritative: tool output can imitate input echoes.
It is never a source of assistant claims. Supply `--input-log` from the driver (one submitted input per line)
to obtain complete counts; uppercase `A` is not counted as allow-once.

The report reuses the eval scoreboard's existing crate-vector-pinned
`newt_conformance` content encoder. `--verify-report` recomputes its CID before
rendering; no mutable history or second hashing implementation is introduced.
Do not publish raw transcripts/verdicts without redaction. The three bundled
fixtures are trimmed real final reports with local paths and repository owners
replaced; terminal endpoints and redraws are omitted.

Offline tests run in `just eval-selftest` and the matching CI lint step:

```bash
python -m unittest discover -s scripts/eval/tests -v
```

Transcript ingestion reads only the last 4 MiB by default; configure this with
`--transcript-tail-mib N` (a positive integer). It seeks past old redraws and
bounds the read to a snapshot of the file size, even while a live log grows.
A cropped first terminal line is discarded. Prompt-echo counts cover only that
tail, are labelled non-authoritative, and remain incomplete; a trusted
`--input-log` supplies independent complete counts. The Linux regression uses
a 256 MiB synthetic file under a fixed 128 MiB address-space limit.

`--transcript` is optional and has no bearing on claims. Without `--session-db`,
claims are always UNGRADED even when raw screen text looks like a final reply.
An otherwise passing report renders `UNGRADED-claims` and exits 1, never PASS;
other failed criteria render `FAIL (UNGRADED-claims)`. Recognized contradictions
in structured assistant text still FAIL. The SQLite fixtures use the production
table DDL and separately populate assistant/user/tool-event fields, including
forged reply/footer text and the sanitized genuine final reply.

Prepared parser execution has its own result boundary. Launch/loader failures,
timeouts, signals, crashes, invalid UTF-8, empty output, and malformed responses
invalidate grading (INVALID, extraction UNGRADED, exit 2), including report replay.
Success requires exit 0, exactly `MATCH` or `NONE` (with an optional final newline),
and empty stderr. A parser rejection remains extraction FAIL only for exit 2,
empty stdout, and one recognized unsupported-syntax/attribute diagnostic on stderr.
Unexpected exit-2 errors, such as unreadable helper input, are infrastructure
failures rather than evidence against the refactor.
