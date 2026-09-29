# Agent toolchain and a smaller Newt tool surface

Status: operator direction accepted, 2026-09-26; migration in progress.

OCAP is the project's distinct contribution and its justified complexity.
Concentrate that complexity in authority composition, attenuation,
enforcement, and audit. Ordinary tools should remain familiar. A custom
tool dialect, workflow ritual, or corrective prompt must not become a
second source of complexity that prevents the model from doing its work.

## Problem and decision

A repository-status question exposed a Git catalog containing only
`branch-list`. The model executed 40 successful branch listings and reached
the round cap without answering. Tool availability was restricted both by
filesystem authority and by prompt disposition; even a session with broad
read authority lost ordinary Git reads on an evidence-only turn. Repeating
successful observations did not trigger the post-write stagnation guard.

The fix is not to grow another Git dialect one operation at a time. The
operator chose an extractable `agent-toolchain` crate, initially in this
workspace, and aggressive removal of special model-facing tools that add
negotiation or block ordinary work. This follows
[hide built-in Git tooling](https://github.com/Gilamonster-Foundation/newt-agent/issues/552)
and the accepted [host-command decision](../decisions/host_command_confinement.md).
[Pi](https://pi.dev/) is a reference for a small core and optional extensions,
not a replacement for Newt's authority or provenance requirements.

## Ownership

| Component | Owns |
| --- | --- |
| Newt | Model loop, session, permission questions, grants, cancellation, display, audit, and contribution attribution |
| agent-toolchain | Portable tool adapters and execution contracts, independent of Newt or its model prompts |
| Platform confinement | Enforcement of filesystem, process, and network authority over the actual child and descendants |
| Brush and its Bridle adapter | Shell semantics and interception; worker supervision, cancellation, bounded capture, and explicit output-loss reporting |
| Optional Git engine | Explicit specialized tool, if retained; never an implicit substitute for a Git command it cannot implement |

The default model surface should use familiar read, write, edit, and command
operations. Operator permission and clarification interactions remain
available. Skills and optional tools provide specialization when requested
or when measured task evidence establishes their benefit. Hiding schemas
while leaving forced redirects and refusal coaching in place is incomplete.

The proposed [portable outline tool](outline-tool.md) is one candidate:
language-aware structure and source spans, available explicitly to the model,
with an independent parsing crate and an OCAP-authorized adapter.

The operator's [Brush fork](https://github.com/hartsock/brush) now carries the
static filter implementation on `main`, merged through [fork PR #9](https://github.com/hartsock/brush/pull/9).
The published prereleases are `brush-ocap-parser` 0.5.0-rc.1,
`brush-ocap-core` 0.6.0-rc.1, `brush-ocap-builtins` 0.3.0-rc.1, and
`brush-ocap-coreutils-builtins` 0.2.0-rc.1. Their registry archives match the
verified packages from commit `2c76eea4b6750def63a0c932a35910e448ee0ef4`.
The fork supplies static filters, final command authorization, typed file-open
policy, owned Unix descriptors, and spawn registration. Its native test jobs
passed on Linux, macOS, and Windows; Unix descriptor transport does not yet
have a Windows counterpart. Newt's published-package and live-assignment
acceptance remain separate from the fork release.

Bridle's wrapper changes are still temporary workspace patches for
`agent-bridle-tool-shell`, `agent-bridle-core`, and `agent-bridle-fdguard`.
External consumers building this checkout's `newt-core` must carry those same
patch declarations, with paths anchored to this checkout, until the Bridle API
changes are released. Cargo does not inherit a dependency workspace's patches
from its lockfile. The Brush dependencies themselves resolve from the registry.

The maintainer [asked for evaluation of the static filter design](https://github.com/reubeno/brush/issues/1183#issuecomment-4620065993)
in [#972](https://github.com/reubeno/brush/pull/972). The operator subsequently
accepted that direction, closed the competing interceptor proposal #1184,
and opened [#1314](https://github.com/reubeno/brush/pull/1314) with the rebase
and `exec`-builtin filter coverage. As reviewed on 2026-09-26, #1314 remains
open and draft. Target its statically registered `CmdExecFilter` and
`SourceFilter` contracts; do not grow a competing Brush interception API.

Port Bridle's real policy with regression evidence for the gaps recorded in
that PR: redirection access intent; a denial that terminates a runaway loop;
original command spelling, resolved executable, and `argv0`; serialization
that cannot silently replace an installed policy with a permissive default;
and final authorization after any command-rewriting filters. A successful
`exec` has no returning post-hook, so audit cannot assume pre/post pairing.
Existing OS confinement remains necessary for external descendants.

Keep Bridle's worker supervision, cancellation, resource lifetime, and result
protocol in its adapter. Newt consumes those contracts instead of recreating
shell behavior or interpreting error strings. The temporary vendored Bridle
adapter is an integration patch to upstream and remove after a compatible
release; these wrapper fixes do not require a second Brush filter mechanism.

## Git contract

Ordinary `git` commands are the interface. Preserve the arguments, selected
repository, stdout, stderr, and exit status. Do not silently drop flags,
translate a status question into branch listing, or call a limited engine
with approximate semantics. The installed Git supplies command behavior;
we do not claim to reimplement Git feature parity.

Observe-only turns still need useful command execution. Enforce their
permitted effects at the execution boundary and ask for additional authority
when required. A prompt classifier must not remove an already authorized
observation solely because the request is phrased as a question. Arbitrary
shell syntax must use the existing structural parser and confined executor;
textual guesses are not an authority boundary.

Preserve attribution, signing, branch protection, destructive-operation
confirmation, and network decisions while migrating. Supporting native
`merge`, `rebase`, or composed commands requires those integrations to work
without bouncing the model to a Newt-only operation. A retained specialized
Git tool is opt-in and accurately declares its limits.

## Current migration status

The current worktree removes native Git read routing and the shell
tool-name redirect. Native reads and staging use ordinary command execution;
the routing table no longer translates status, log, diff, or branch arguments
into the embedded engine. Regression fixtures exercise actual temporary
repositories, explicit grants, and the absence of embedded dispatch. These
source changes do not establish the two live acceptance tasks or platform
parity by themselves.

On Windows, the restricted AppContainer backend is a declared exception to
native Git availability. After the ordinary filesystem and executable-authority
checks succeed, a single literal direct Git program is refused before it is
spawned when that backend is selected: Git for Windows resolves the current
directory through ancestors outside the admitted roots. The result names the
unavailable backend and confirms that no command ran; Newt neither widens those
roots nor retries on the host. Compound or dynamic shell source retains its
existing confined-executor semantics, and an operator can deliberately choose
the existing non-AppContainer route with `--disable-ocap` or `--full-access`.

The embedded schema is absent from both the default advertisement and
`tool_search`. Its engine and dispatch arm remain internal dependencies.
That is a migration state, not complete removal or a fully separate plugin.

The live repository-status test now completes using one ordinary Git command.
It also exposed a false claim warning: the word "committed" in a negated
statement caused unchanged HEAD to refute the answer. That keyword-based
accusation and its unused Git probes have been removed. Actual branch, path,
and disclosure checks remain; unchanged HEAD does not establish whether a
test, push of an existing commit, or read-only task succeeded.

The Lab test also exposed generic repair steering that treated an invalid
working directory as a code defect and kept demanding edits after successful
observations. Those unsolicited repair-lock and rediscovery prompts have been
removed. Tool results remain available to the model; actual progress, repeated
failure evidence, explicit budgets, and authority enforcement remain separate
runtime controls. No error-string exception or new repair protocol replaces
the removed steering.

### Native executable permission recovery: remaining gap

The live Lab run found installed `mv` and `rm` unavailable in the selected
carried userland. This is an execution-grant recovery gap: the shell returns
exit 127 without a structured denial, and Newt's installed-program advice
does not enter the permission gate. The model can request the named executable
explicitly, but ordinary command execution should surface that operator
decision itself.

The fix belongs in `agent-bridle-tool-shell`: expose a nonexecuting, typed
execution-requirements preflight derived from the same confined builtin and
carried-command registries, cwd, PATH, and environment used for execution.
Newt can pass missing executable requirements to its existing permission
gate, then execute the original source once. The current public structural
inspection does not expose that classification. This API is not implemented
by this change.

Known build programs now use the existing Build capability before a shell
pipeline starts. Structural inspection selects that capability; the operator
approves its calibrated toolchain reads, workspace writes, and private scratch,
while network authority remains the incoming invocation's grant. The original
shell source then runs once with that environment and the approved filesystem
fence. Denying Build prevents every stage and redirection. Real macOS fixtures
cover Cargo build pipelines, native `cargo fmt`, denied execution, and sibling
filesystem denial. This fixes the observed Lab failure without claiming the
general executable-resolution API above or native Windows/Linux test parity.

Do not infer new authority from arbitrary command stderr or replay a compound
command after some earlier stages may already have written files. Acceptance
requires denied preflight to have zero effects, approved execution to occur
once, unchanged filesystem/network grants, no spurious executable request for
builtins, and no guessed grants for missing programs or dynamic commands.

The native-commit candidate replaces the blanket `git commit` refusal with
the scoped adapter below. Its real regression starts with Knowledge's exact
`git add .gitignore && git commit -m ...` command. This is an implementation
under validation, not evidence of installation or live task completion.
Merge, cherry-pick, revert, and rebase remain explicitly guarded until their
multi-commit lifecycles have equivalent attribution and publication coverage.

A transitional native-command preflight preserves branch-delete and
stash-drop/clear confirmation and rejects recognized protected-ref,
ref-overwrite, and unresolved destructive forms in successfully inspected
commands before execution. Ordinary branch creation, switching, queries,
and unclassified Git verbs retain native execution and its filesystem,
process, and network authority checks. This preflight is not a command
allowlist and leaves admitted shell source unchanged.
This is deliberately bounded to inspectable commands, not an arbitrary
program-execution boundary: failed inspection defers to the existing confined
executor. Scripts, runtime-selected executables, aliases,
repository redirection, concurrent repository changes, and metadata writes
by other programs exceed this preflight's coverage. The scoped commit adapter
does not turn it into a universal Git metadata boundary. Do not claim full
mutation parity or OCAP enforcement from a command preflight alone.

## Extraction sequence and removal gates

1. Move existing adapter and authority contracts to an independent crate,
   keeping their tests and re-exporting them during migration. Keep the
   legacy model schema behind an explicit `embedded-git` feature. This
   first slice changes ownership, not runtime behavior.
2. Wire the native command adapter through the existing confined executor,
   permission gate, cancellation, and audit funnel. Prove literal Git
   behavior with real temporary repositories, including status, staged and
   unstaged changes, untracked files, linked worktrees, flags, paths with
   spaces, and error exits. Verify attribution and destructive-operation
   gates before enabling all mutation paths.
3. Remove default embedded Git advertisement, command redirects, refusal
   coaching, and stale prompt instructions together. Remove unused code;
   retain an optional engine only for a demonstrated consumer. Newt's core
   must not gain another tool negotiation protocol from this extraction.
4. Inventory the remaining tool catalog by live use and capability overlap.
   Delete redundant default tools after their ordinary-command replacements
   pass effect, output, and authority tests. Keep useful internal services
without requiring the model to invoke them as separate tools.
5. Replace fixed round exhaustion during useful work with progress-based
   continuation. New evidence, actual workspace changes, and verification
   count; identical successful observations and narration do not. Honor
   cancellation, explicit budgets, and permission decisions throughout.

## Native commit adapter: scoped implementation

`agent-toolchain::native_git` owns the portable finalization, signature, and
publication state. Newt supplies its existing canonical `CommitAttribution`
finalizer and host-held `CommitSigner` through the internal `GitTool` seam.
The model continues to use `run_command`; there is no new model tool schema.

An inspected commit-bearing command uses Brush's runtime external-command
filter. The entire original shell source runs once, with its stdin, output,
cwd, and effective authority. No partly executed command is replayed after a
broker denial. Native Git still parses commit options and pathspecs. Protected
global config adds the helper path and configured signer; explicit
`--no-gpg-sign` retains native parsing and fails the required-signature check
before the ref is published. The broker never reconstructs a message from
`-m`, `-F`, or shell syntax.

The private Bridle channel binds each callback to the actual admitted Git
PID, process birth, and executable image. The helper is the current trusted
Newt executable; repository hooks receive no broker descriptors. Canonical
content-addressed transport records link each response to its exact request.
On macOS, the `/usr/bin/git` exec transition is explicitly limited to the
already selected developer toolchain's Git image, using the existing trusted
resolver and runtime read closure.

Helpers run from a flat read-only directory outside all granted write roots.
The directory contains only aliases to the current executable. A filesystem
`All` write grant, overlapping scratch root, or authority able to relocate
the helper directory cannot protect that mechanism and is refused. The
supported operator choice is directory-scoped workspace authority. Keys remain
host-held and outside the child's filesystem read grant.

Git's [message and reference transaction hooks](https://git-scm.com/docs/githooks)
invoke the existing finalizer and validate the exact candidate before
publication. The host's repository probes use the same confined executor and
effective grant. Commit parents must describe an append or amendment of the
admitted tip, and the old ref must match that snapshot. The final candidate
must retain canonical attribution and the exact approved signature. Only a
verified committed ref and matching actual object consume contributors and
increment the existing success counter. Aborts and failed signing consume
nothing. Commit/helper payloads are bounded to 256 KiB.

Existing shell hardening's hook policy remains in force. An explicit
operator/caller hooks path is chained inside the same filesystem fence,
with all private descriptors scrubbed. The protected final helper path is
injected after caller global config. There is no post-commit amendment or
temporarily published unsigned object.

This boundary covers supported native `git commit` invocations, including
ordinary compositions. Directory authority also permits changing `.git`
with other programs. This implementation does **not** claim those arbitrary
writes, dynamically invoked descendant Git, or arbitrary aliases are brokered
commits. It does not establish complete Git mutation parity. The native macOS
process/fence fixture passes against the published Brush packages. Native Linux
still needs that actual-effect validation, and the authenticated helper
transport is unavailable on native Windows, whose implementation remains a
parity requirement.

The passing native macOS acceptance fixture checks the Knowledge compound,
literal `-m --`, refusal to publish when signing is disabled, preserved author intent,
repository-hook attempts to unlink a protected helper or inherit its endpoint,
the absolute Apple Git shim, refusal of unprotectable global write scope,
the existing default-branch policy, relative hook paths, and `post-index-change`.
Default-branch decisions share the embedded adapter's canonical rule; native
repository observations use the admitted read scope. Each prepared command
freezes its contributor cursor. The session observes confirmed native commits
through the existing typed success counter, even if a later compound stage
fails or the outer tool future is cancelled; shell text is never that signal.
Merge/rebase families, broader linked-worktree coverage, editor/cancellation
cases, and native Windows remain separate gates before expanding that scope.

## Acceptance tasks

The existing Herdr `knowledge` assignment must answer whether the repository
has uncommitted files, with correct tracked/staged/untracked distinctions.
The existing `lab` assignment must refactor the largest code file and run
the relevant verification within its filesystem grant. Run both with normal
task prompts and normal round settings, without operator `continue`, manual
round increases, or corrective tool-use prompts. Permission or clarification
questions are valid outcomes when genuinely necessary; missing harness
capabilities are defects to fix, not handoffs to the operator.

Keep transcripts, actual tool invocations, build identity, exit status,
usage totals, and resulting repository diffs as acceptance evidence. A
successful crate test or a terminal "done" message alone does not satisfy
these tasks.

Workspace test-runner discovery is advisory. The conclusion gate retains
checks explicitly requested in the task and checks actually requested or
attempted during the turn, including their failures and stale results. A Rust
edit does not make an unrelated Python package's tests mandatory merely
because both exist in the checkout. Repository instructions and the model's
assessment still determine relevant verification. Reports remain model-authored
Markdown: table labels are not tool identities or independent attestations.

## Cross-platform and network requirements

Linux, macOS, and native Windows share the model interface and behavioral
fixtures. Platform implementations may differ, but WSL does not establish
native Windows parity. Test real processes on each platform, including
permission denial, descendant confinement, cancellation, and helper failure.

The [network console design](network-console.md) remains applicable:
allow-all is an authority decision, not an audit opt-out. Network activity
and inference usage stay observable; filesystem authority is independent.
Unsupported confinement must become a concrete operator setup or permission
decision before work starts, never a repeated model repair loop.

### Build temporary storage and terminal tests

Build approval includes the exact workspace partition of a managed temporary
directory outside Git worktrees. Unrelated test fixtures must not inherit the
assignment repository's Git identity. The proposed path is stable for session
approval; allocation happens only after admission. Per-run leases retain it
until execution and cancelled descendants finish, then remove owned storage.
The operator's existing build-scratch override still takes precedence.

On macOS, Newt explicitly enables Bridle's `macos_private_ptys` runtime option
(Bridle defaults it off). Seatbelt permits the PTY allocator and slave devices
carrying the kernel's `com.apple.sandbox.pty` extension. An ambient `/dev` read
base does not grant other slave terminals; explicit operator read grants still
apply. The real Build fixture checks new-terminal I/O, an unrelated host
terminal's read/write denial, and the disabled-policy control. No network or
exec scope is widened. This is a declared private-device runtime exception,
not an improvement to Seatbelt's existing partial resolved-authority model.
The existing fence CID does not include `SandboxPolicy` or the complete SBPL
ruleset, so it does not attest this option; build identity and native evidence
must accompany this mechanism's audit.

That proof is specific to macOS. Linux requires separate private-devpts or
equivalent device isolation; Windows requires ConPTY/handle evidence. Neither
is established by this change. Inherited terminal descriptors and `/dev/tty`
remain separate surfaces; this does not claim universal terminal isolation.

## Provenance boundary

The extraction reuses published authority types and existing session audit.
Native Git helper values use `content-addressable` canonical DAG-CBOR; Bridle's
bounded RPC envelopes derive IDs with the same dependency and verify request
and response links on the production path. There is no parallel durable event
store or custom digest scheme. Git retains its native content-addressed commit
history and reflogs. Failed/aborted preparations do not mutate contributor
history; confirmed commits use the existing ledger lifecycle. A crate move is
reversible in version control and does not rewrite existing session history.
