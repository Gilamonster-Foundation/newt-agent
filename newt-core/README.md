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
replace the tool gates or cancellation and round-budget limits. Recurring plan
reminders show progress and step indexes as agent-maintained, advisory state.
Step descriptions remain in the source plan rather than being repeated as host
claims about tool availability or operator decisions.

Recovered capability claims are historical context, not current observations.
Explicit claims of freshly completed toolchain probes or CI attempts receive one
bounded correction when this turn has no returned execution attempt. The correction
uses current tool schemas and preserves permission gates; an unsupported final
claim carries a visible harness evidence notice. Failed and denied attempts
count as attempts, but do not prove availability or the claimed command result.
This narrow check covers the observed probe wording, not arbitrary prose.
Receipt-shaped lifecycle denials also require an observed execution or permission
request. Lifecycle discovery and unavailable-run results advertise the explicit
offline build action. Host-present shell absence guidance distinguishes this
project-validation route from per-binary direct execution grants, which do not
grant compiler descendants or their filesystem access. This guidance never
retries or grants authority itself.

Smart Harness also retains native execution outcomes in Agent Frame's existing
verified occurrence journal, before display and independently of optional turn
telemetry. Classification and recovery nudges receive bounded current-turn
facts derived from that journal, with references to the actual occurrences and
returns. Model-authored plans and summaries cannot establish these outcomes.
An absent historical record remains unknown; only an explicitly accounted turn
can establish zero calls. A recorded denial does not itself identify an operator
decision, and a direct executable denial does not establish a confined Build
denial. This evidence supplies context without changing authority or guaranteeing
that the model chooses an available action.

Navigation selections also accept a complete bare or `json` Markdown fence
around the CID array. The raw auxiliary reply stays recorded; unknown CIDs,
missing protected inputs, split tool exchanges, and over-budget selections
remain refused. Prose and incomplete fences are not repaired, and classifier
verdicts retain their strict JSON-string protocol.

When a navigation selection includes a tool call, the host adds the generated
result that frames it, so the model does not have to name harness-generated
entries. A selection that omits the call itself is still refused, and the
completed selection is validated against the byte budget like any other. The
catalog prices that in: a card's `bytes` include the generated results its
exchange brings, `max_bytes` is what is left after pinned entries and the
re-read pointer, and an exchange is offered whole or not at all, so a selection
that adds up is not refused for size or for a missing half.

A successful built-in permission grant releases cached capability failures and
typed failed native shell/lifecycle executions so the original operation can be
checked again. A native execution receipt permits re-evaluation; it does not
prove that permissions caused the failure. Refused or unavailable permission
requests leave the cache unchanged. Untyped native error text and ordinary
failures from other tools remain cached; every retry still passes through the
normal tool gate, and executed-failure history is retained.

For compiler/test subprocesses that cannot run under a literal executable grant,
call `lifecycle` with `{"phase":"test","action":"build"}` (or `check`, `lint`,
`format`). This requests explicit, once-only confined build authority. The
resolved command and calibrated read roots are shown before approval. Build
scripts and tests inherit workspace-only writes and denied network access;
Cargo uses installed toolchains, cached dependencies and a workspace-local
`target`. The ordinary `run` action and raw shell grants remain unchanged.
Restricted preset/delegated ceilings and Smart Harness frame isolation still
apply. Dependencies must already be cached; this action does not authorize
network installation or shared-cache writes. Network denial includes loopback:
tests that start localhost servers need a separately authorized execution
policy. This action does not promise that a project's entire test suite can
run offline.

Confined shell and lifecycle calls refresh standing session and verified durable
grants before starting a child, so a grant applies to the next call in the same
turn. Refresh starts from the caller's baseline before adding standing grants
and retains preset, delegation, denial, and private-frame boundaries;
it never consumes or publishes pending allow-once grants.

For native filesystem access, `run_command` accepts optional `fs_read` and
`fs_write` arrays of absolute paths. These request additions for that invocation
and can consume matching pending allow-once approvals from `request_permissions`.
Unrelated calls leave those approvals pending. All paths are validated before
approval; call-local authority survives later executable or network approval,
while incomplete returned authority prevents the child from starting. Existing
directory containment, caller bounds, and permission ceilings still apply.
An invocation can consume approvals even if a later approval is refused; it
never publishes them for later calls. Native stderr does not infer authority.

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

Shell and lifecycle child environments include the parent's `TZ` by default,
alongside `HOME` and `USER`. The value passes through unchanged: absent stays
absent, and explicit empty stays empty. A custom `[shell] env_passthrough` list
can omit `TZ`; unrelated environment variables remain excluded. The operating
system/runtime interprets the timezone, and a `TZ` file path grants no filesystem
access. Stdio MCP children use the same timezone default with per-server
environment overrides retaining precedence.

It also hosts the shared agentic tool executor used by the TUI and headless
paths. Built-in file tools include `read_file`, `write_file`, `edit_file`,
`delete_file`, `list_dir`, and `find`, all mediated by the same caveat and
prompted-permission checks.

`tool_search` discovers tools by keyword and loads a hidden schema when queried
with its exact name. The schema appears on the next provider request; search
never executes the operation or grants permission. At the 128-function wire
limit, the shared exposure controller replaces the oldest optional schema,
preserving Kernel tools and queued activations. Evicted schemas stay searchable
and can be loaded again. Fuzzy, unknown, and already-loaded searches do not
change the tool list. Normal permission and context-budget checks still apply.

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

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 19:26 EDT | Date: 2026-09-17

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 21:43 EDT | Date: 2026-09-17

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 22:45 EDT | Date: 2026-09-17
