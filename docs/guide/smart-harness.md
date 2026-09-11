# Smart harness

Smart mode records a content-addressed session and uses auxiliary model calls
to select retained context and classify tool-less replies. Enable it with
`newt solve --smart-harness --instruction-file task.md` or workspace configuration:

```toml
[smart_harness]
enabled = true
# Optional private directory outside all tool filesystem grants:
# frame_dir = "/private/newt-frames/project"
# Optional embedded palette alias or explicit GGUF path:
# model = "installed-palette-alias"
# model_path = "/models/auxiliary.gguf"

[smart_harness.adjudication]
timeout_ms = 30000
max_calls = 8
total_timeout_ms = 60000
max_input_bytes = 262144
max_output_bytes = 32768
max_output_tokens = 4096
```

The default auxiliary runs an installed embedded model on CPU and pins immutable
model and tokenizer bytes for the backend's lifetime. Choose an external backend
for non-CPU smart inference. Startup errors are visible; there is no silent
fallback. Its call, byte, token, and elapsed-time limits are
accounted separately from primary tool rounds. Both narration classification
and context selection share these auxiliary limits. Protocol instructions and
the continuation nudge are also overridable under
`smart_harness.adjudication`.

The [recorded CPU comparison](../../newt-inference/tests/fixtures/README.md)
found no valid verdicts from the installed 0.5B model with the initial bundled
protocol, even with a longer deadline. These measurements do not establish a
useful default classifier. The external model's 8/8 result reported in
[PR #2263](https://github.com/Gilamonster-Foundation/newt-agent/pull/2263) covers
eight curated fixtures; it does not establish general classification quality or
superiority. Smart classification remains experimental; malformed replies
produce an explicit failure.

An external auxiliary requires an explicit endpoint, protocol, model, and a
nonempty `device` placement label (`cpu`, `cuda`, ...). The host rejects an
incomplete override. Placement is recorded as `operator-declared`; the client
does not verify the server's hardware. The auxiliary may use the primary's
endpoint and model. The manifest records `shares_primary_origin` to identify
whether their URL origins match. Shared servers or hardware can contend for
resources; neither the placement label nor separate budgets guarantees physical
independence. An unavailable auxiliary produces an error without silently
selecting another backend.

```toml
[smart_harness]
enabled = true
device = "cpu"

[smart_harness.backend]
endpoint = "http://127.0.0.1:11435"
kind = "ollama"
model = "installed-auxiliary-model"
```

To judge with the same model that does the work, point the auxiliary at the
primary's own server (placeholder host shown):

```toml
[smart_harness]
enabled = true
device = "cuda"

[smart_harness.backend]
endpoint = "http://inference.example:8080"
kind = "openai"
model = "the-primary-model"
# api_key_env = "AUXILIARY_API_KEY"  # only if that server requires it
```

Additional `[smart_harness]` limits are `max_catalog_entries`,
`max_fetched_bytes`, `max_dereferences`, `max_navigation_calls`, `max_retries`,
`max_elapsed_ms`, `max_history_nodes`, `max_record_bytes`, and `max_slice_bytes`.
`max_record_bytes` defaults to 64 MiB per source or structured record and applies
to both writes and reads, including restoration and replay. The resolved
auxiliary identity, placement evidence, protocol instructions, and budgets are
committed in the session configuration. The solve contract includes this
configuration, invocation mode, starting CID, and final head CID.

Smart solves start a fresh resumable session by default. Use
`--frame-dir PATH` to select its storage directory and
`--resume-from HEAD_CID` to restore one explicitly. Restore checks the complete
record closure and requires the current canonical workspace, private storage
directory, current tool caveats, auxiliary configuration, and budgets to match.
A record never grants authority. Provider protocol changes also require a
fresh session.

A writable session holds an exclusive OS lock on its run from creation or
restoration until release. Resume accepts only the current `heads/<run CID>`
checkpoint; an older checkpoint reports a conflict without selecting a divergent
history. Stop the active writer and use the current locator to continue. To
start independent work, create a fresh run explicitly. Read-only frame
inspection does not acquire execution ownership. Never remove the persistent
`locks/<run CID>` files to resolve a conflict. The OS releases the lock when its
last handle closes; a forked child must drop any inherited session handle too.
A failed journal publication stops further writes from that session. Release
it and restore the actual locator; a failure after replacement may have made a
new checkpoint visible even though the append could not confirm durability.

Tool batches are recorded before execution, with a separate occurrence identity
for each call. The host records a start before dispatch and retains the exact
disclosed return before display, bounded projection, or another tool. The later
model-facing envelope has its own recorded provenance. The raw result
is recorded before resolving any spill hints; verified external sources
are attached afterward. A malformed or unavailable hint stops the current batch
while preserving the raw return as observed. An ordinary returned
string, including `Error: ...`, does not prove success or failure. Rust and
Python hosts with an actual typed tool error can explicitly record `Failed`;
frame persistence errors remain session errors.

After an interruption, completed returns remain available. A call with a start
but no observed return is `Uncertain`: it may have had side effects. An admitted
call that never started is `NotStarted`. Resume fills missing provider result
slots with explicit harness notices before a new operator message; a retained
return without its finished presentation gets a source pointer. Recovery never
reruns a tool and does not promise exactly-once external effects. The current
native provider loops execute calls sequentially.

Sessions now write schema 2. This unmerged experimental interface explicitly
refuses writable restoration of schema 1 because its missing per-call records
cannot establish what executed. Start a new run; read-only frame inspection and
the existing exact request-replay format remain available for older records.
See the [reusable storage contract](../../agent-harness/README.md) for filesystem,
process lifetime, and Unix/Windows crash-durability limits.

Frames default to `frame/<workspace CID>` under Newt's user configuration
directory (`~/.newt`, or `NEWT_CONFIG_DIR`). The workspace CID addresses the
canonical workspace path. The resolved directory is included in the solve
contract's `configuration.authority_context.frame_directory`.
It must remain outside every model read and write grant, including implicit
executor paths and path aliases. Storage beneath the workspace, an overlapping
`--read` or `--write` grant, and unconfined launch modes are refused. Permission
prompts cannot grant access to the frame. The host mediates retained context
through `re_read`; ordinary workspace tools retain their scoped access.
Grant roots must also remain stable while the sandbox starts: a root that the
model can replace through a writable ancestor is refused, including intermediate
symlink targets and redundant child grants beneath a writable workspace.
Grant strings containing `..` are refused. Use stable workspace or external
roots directly.

Newt's durable smart runtime currently requires Linux with Landlock and the
object-bound native filesystem tools. Startup refuses weaker execution modes.
Local MCP processes are checked against the frame boundary before they start;
remote MCP servers remain separate, operator-configured authorities. A foreign
Rust or Python host must enforce its own file and subprocess boundary; the
reusable session library does not install a sandbox.

Smart mode refuses crew execution and native `find` because those adapters do
not retain protected filesystem access throughout their reads. Use
`run_command` for searches in the confined shell. Native Git retains its existing
scoped `branch-list` surface; other permitted Git commands use the confined
shell and may need explicit grants for Git configuration files. The existing
refusal of shell-created commits still applies, so this
mode does not currently provide a commit path. These restrictions do not apply
to the reusable frame and session APIs themselves.

`--hermetic` admits the current task, system instructions, tool definitions,
workspace, configuration, and auxiliary model. It excludes inherited
conversation and ambient memory inputs and cannot be combined with
`--resume-from`. This constrains admitted input sources; it does not promise
deterministic model sampling, unchanged workspace files, or network responses.

The TUI uses smart mode when configuration enables it. Context is selected from
retained originals before each request; legacy history summaries and close-time
summary extraction are disabled. `/compact` explains this behavior instead of
creating an untracked summary. Each conversation owns its session; changing its
authority or committed auxiliary settings requires a fresh conversation. On
restart its stored run locator follows the latest verified checkpoint. An older
conversation without an accounted frame is refused visibly; start a new
conversation or resume an explicit frame CID through `newt solve`.

Each live TUI conversation retains its embedded auxiliary and the exact model
and tokenizer bytes pinned when it opened. Later turns reuse those assets after
checking current authority, configuration, and primary protocol. Replacing or
removing the original files does not change that live conversation; a new
conversation pins the selected files afresh. Changing model selection or
authority requires a new conversation. External auxiliary credentials are
checked for availability on each turn.

An `Answer` remains subject to task-evidence checks, disclosure filtering, and
finalization; a missing required check can continue the turn. A `Question` keeps
the original question and reports `awaiting_operator`. Exhausted narration
reports `incomplete`; malformed or unavailable adjudication reports failure.
These are provenance-checked classifications, not proofs that an answer is true
or the requested work is correct. The process exit contract remains unchanged:
automation must inspect structured `status`, `end_reason`, and `outcome` to
distinguish task completion from a cleanly terminated invocation.

Use the resolved private directory as `FRAME_DIR` below. Inspect the emitted head
with `newt frame explain HEAD_CID --frame "$FRAME_DIR"`
or `newt frame parents HEAD_CID --frame "$FRAME_DIR"`. Each command verifies the
selected record and its immediate references without walking ancestry or
granting authority; its report explicitly distinguishes this check from full
session admission. Follow the reported journal, reply, verdict, request, and
projection CIDs one command at a time. `--max-bytes` and `--max-references` bound
these reads. Once a request CID is known,
`newt frame replay REQUEST_CID --frame "$FRAME_DIR"` reconstructs and verifies
the exact retained request bytes before writing any output. CLI replay uses
the store's default 64 MiB per-record cap. For a session configured above that
cap, use the Rust or Python session API with the same explicit configuration.
