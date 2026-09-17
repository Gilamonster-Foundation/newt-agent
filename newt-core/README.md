# newt-core

Newt-Agent core types, errors, and the NeMoCode-style tier router.

The router is the NeMoCode inheritance: it classifies an incoming turn into a
`Tier` (FAST / STANDARD / COMPLEX / REVIEW) and asks the configured backends
which can serve that tier. The crate also carries the shared configuration
model (`~/.newt/config.toml`), session and memory types, MCP server
resolution, metrics, and capability caveat extensions used across the
workspace.

Turn metrics distinguish an answer from a question awaiting the operator and
from incomplete narration. `TurnEndReason::AwaitingOperator` serializes as
`awaiting_operator`; both narration exits report incomplete work. These are
control outcomes, not evidence that a claimed external action succeeded.
Pending-action matching keeps whole-reply similarity for paraphrases and boosts
prototype recall only when its configured opening phrase occurs in the reply.
This reduces false narration warnings from shared nouns in completed reports.

Saved plans and workflow reminders guide execution; newer operator instructions
can correct the plan or request a stop or report-only response. Recovery guidance
allows an exact permission request when the operator has not declined it and a
grant is available, or an alternative within existing authority. An unresolved
blocker remains incomplete work. These reminders neither grant permissions nor
replace the tool gates or cancellation and round-budget limits.

A successful built-in permission grant releases cached capability failures and
typed failed native shell/lifecycle executions so the original operation can be
checked again. A native execution receipt permits re-evaluation; it does not
prove that permissions caused the failure. Refused or unavailable permission
requests leave the cache unchanged. Untyped native error text and ordinary
failures from other tools remain cached; every retry still passes through the
normal tool gate, and executed-failure history is retained.

Confined shell and lifecycle calls refresh standing session and verified durable
grants before starting a child, so a grant applies to the next call in the same
turn. Refresh starts from the caller's baseline before adding standing grants
and retains preset, delegation, denial, and private-frame boundaries;
it never consumes or publishes pending allow-once grants. Native child filesystem
errors still require an explicit permission request, and an allow-once filesystem
grant has no new consumption path here.

Verification uses the existing bounded workspace snapshot to avoid requiring
unrelated code suites for an unchanged workspace, such as a permission-only
turn or a report written elsewhere. Explicitly named checks and attempted
checks remain eligible, including failed and denied runs. A changed or unknown
workspace remains conservative. Isolated `.worktrees` are excluded; workspace
documents and Newt instructions still affect verification state.

File-tool schemas accept absolute or workspace-relative paths within granted
access and tell the model to preserve supplied absolute paths. Final path checks
distinguish missing workspace files from unverified external references. The
checker retains its lexical boundary and existing symlink behavior; it does not
inspect lexically external paths or establish completion of external edits.

It also hosts the shared agentic tool executor used by the TUI and headless
paths. Built-in file tools include `read_file`, `write_file`, `edit_file`,
`delete_file`, `list_dir`, and `find`, all mediated by the same caveat and
prompted-permission checks.

File mutation results use NewtUI's shared diff model and Markdown projection
for authorized UTF-8 changes observed around each operation. Verification runs
before an optional build check, so its failure does not erase the tool's diff.
Unavailable preimages and receipt limits (256 KiB per version, 4096 total lines)
are named explicitly without emitting partial patches. Durable observations
retain their disclosure and secret-redaction policies. The session-only spill
archive keeps its terminal sanitization and size limits; it does not promise
secret redaction. Retained text is not a byte-exact patch export. See
[Step 13.2](../docs/ROADMAP.md#step-132--observed-file-changes-in-tool-results).
Terminal displays visibly escape source controls and identify that projection;
the canonical returned result keeps the source bytes.

Rich displays receive a one-shot typed hint tied to the exact receipt in that
raw result. NewtUI supplies safe numbered source cells; the optional existing
Syntect parser supplies full-file syntax foregrounds without reinserting raw
source. The shared Markdown renderer uses the same parser. Unsupported glyphs
are visibly substituted, and the styled view is identified as unsuitable for
raw patch application. Text observations and spill retention keep their
existing limits and redaction behavior.

Harness-owned Git subprocesses share `git_hardening::hardened_git`, which
returns a fallible command builder with repository config gadgets disabled and
the child environment scrubbed. On macOS it resolves Git from PATH before
spawning, avoiding the fork/pre-exec path in parallel launches. Missing or
unusable PATH fails closed; it does not substitute another Git installation.

Interactive front ends may inject the public `LiveToolOutput` interface into
`ChatCtx` to observe streaming shell bytes without changing the authoritative
tool result. Newt dispatches those bytes through a bounded presentation queue,
contains observer panics, and runs renderer startup off the tool-execution
task. Normal completion and responsive cancellation close the generation
before the canonical completed result is rendered. A bounded teardown timeout
instead calls the sink's no-output `abandon` transition synchronously,
invalidating delayed callbacks before canonical rendering resumes. Headless
callers set `live_tool_output` to `None`, preserving completion-only output.
Each tool invocation gets a new generation, so late bytes from an ended or
retried invocation are ignored. The front end owns display sanitization and
retained-history policy; neither can mutate the model-facing result.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: Shawn Hartsock | Time: 21:03 EDT | Date: 2026-09-16

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 15:24 EDT | Date: 2026-09-17
