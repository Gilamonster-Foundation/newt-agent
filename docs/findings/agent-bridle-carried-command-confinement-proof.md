# Proving That an Allowed Carried Command Cannot Escape Agent Bridle

**Repository:** `Gilamonster-Foundation/agent-bridle`  
**Target:** `BrushShellTool` with `carried-coreutils`  
**Original finding date:** 2026-07-23  
**Status:** Historical pre-remediation proof input, with Agent Bridle 0.7.14
implementation status added 2026-07-24

> [!IMPORTANT]
> Sections 1–12 preserve the threat model, proposed architecture, unchecked
> acceptance list, and source notes as they were written against the
> 2026-07-23 pre-remediation implementation. In those sections, “currently,”
> “urgent,” “expected current result,” and unchecked `[ ]` boxes describe that
> historical baseline. They are not claims about Agent Bridle 0.7.14 and are
> not a current release checklist. The status immediately below records the
> implemented and verified 0.7.14 boundary without rewriting the original proof
> input after the fact.

## Agent Bridle 0.7.14 post-fix status

The 0.7.14 implementation closes the carried-command filesystem gap identified
by this document:

- Brush and carried utilities run behind an authenticated, same-image private
  worker-control channel on Linux and macOS. The worker and carried descendants
  inherit the selected native L3 boundary before untrusted shell evaluation.
- Restricted filesystem axes require kernel enforcement and fail closed on a
  downgrade. Successful envelopes report the achieved sandbox and per-axis
  enforcement instead of presenting the carried path as
  `SandboxKind::None`.
- The public facade exposes
  `Registry::dispatch_with_strength_floor(...)`, so an embedder can bind the
  enforcement strength reviewed for an exact operation to the actual dispatch.
  The ordinary `dispatch(...)` path retains its advisory default.
- The private worker entry point is no longer a public `dispatch_host` binary.
  Same-image authentication, request binding, and platform capability checks
  prevent an unauthenticated caller from manufacturing the worker transition.
- Platforms without that authenticated private-control mechanism fail closed.
  In particular, Windows does not advertise Brush and the facade falls back to
  the structurally restricted `ShellTool`.

Verification completed for the 0.7.14 handoff:

- 127 `agent-bridle-core` tests and 50 shell-tool tests;
- real Brush and carried-coreutils harnesses, including allowed/denied
  filesystem behavior;
- private-control forgery and substitution regressions, PS4 injection,
  enforcement-downgrade, facade, and platform-selection regressions;
- targeted Linux and Windows cross-checks and the macOS Seatbelt test;
- formatting, Clippy with `-D warnings`, packaging, and diff checks.

The remaining boundary is intentionally narrower than “all shell descendants
are mediated by Brush.” An admitted external program can create grandchildren
that do not re-enter Brush’s L2 interceptor. Embedders must therefore require a
kernel strength floor for plans containing external programs; an advisory floor
is suitable only for carried/in-worker-only plans, while Agent Bridle
independently continues to require kernel enforcement for restricted
filesystem axes. This addendum does not claim that every aspirational matrix
item in the historical checklist below was implemented, nor does it expand the
filesystem proof into Linux program-identity or hostname-network confinement.

## Executive decision

An allowed carried command must not be trusted merely because:

1. Brush admitted the command name;
2. the command was compiled into the Agent Bridle binary; or
3. the `before_exec` interceptor observed the re-exec.

Those facts establish **command admission**, not **confinement of the command's internal behavior**.

A carried `cat`, `head`, `cp`, `dd`, or `sha256sum` opens files inside its own process. Those opens do not pass through Brush's `before_open` hook. Therefore the security claim must be established below Brush, at the operating-system boundary.

The required architecture is:

> Run the entire Brush worker and every carried-command descendant inside an inherited, kernel-enforced filesystem sandbox derived from the effective `Caveats`. Refuse the invocation when a restricted filesystem axis cannot be enforced.

For Linux, the immediate implementation target is Landlock. The preferred cross-platform shape is a dedicated sandboxed Brush worker process, launched through the same confinement machinery already used by `ConfinedCommand`.

The proof is deliberately limited to the filesystem axes:

- `fs_read`
- `fs_write`

It does **not** prove complete Linux program-identity confinement for `exec`. Agent Bridle's own ADR 0011 correctly documents the loader/interpreter trampoline that prevents Landlock from making that stronger claim.

---

## 1. The urgent gap at the pre-remediation baseline

At the 2026-07-23 baseline, the carried Brush engine described itself as an L2,
advisory boundary:

- `before_exec` observes resolved external commands;
- `before_open` observes paths opened by Brush itself;
- `sandbox_kind = None`;
- the `exec` builtin is removed because it bypasses the interceptor funnel.

The carried coreutils shim then re-executes the host binary:

```text
<current-exe> --invoke-bundled <command> <args...>
```

The new child calls the selected uutils `uumain` function.

This is good dispatch plumbing, but it does not by itself confine what `uumain` opens. For example:

```sh
cat /outside/secret
sha256sum /outside/secret
cp /outside/secret /allowed/copy
dd if=/outside/secret of=/allowed/copy
```

Brush can admit `cat` while the child independently calls `openat(2)` on `/outside/secret`.

The existing `agent-bridle-core::ConfinedCommand` already contains the essential L3 pattern:

1. check command admission;
2. select the best available sandbox;
3. fail closed when a restricted axis is not enforceable;
4. apply Landlock on a fresh throwaway thread;
5. spawn the child on that thread;
6. let every descendant inherit the Landlock domain;
7. report the sandbox that actually governed the child.

The urgent fix is to make the carried Brush path inherit this property instead of ending at the L2 interceptor.

---

## 2. Security theorem

Let:

- \( C \) be the effective `Caveats` minted by `Gate::authorize`;
- \( R(C) \) be the set of filesystem read roots granted by `C.fs_read`, plus the explicitly disclosed runtime loader/data roots;
- \( W(C) \) be the set of filesystem write roots granted by `C.fs_write`, plus narrowly defined device sinks such as `/dev/null`;
- \( P \) be the Brush worker and every process descended from it;
- \( op(p, x) \) be a filesystem operation attempted by process \( p \) on object \( x \).

The target claim is:

```text
For every p in P:

  read-like(op)  and x not in R(C)  => the kernel denies op
  write-like(op) and x not in W(C)  => the kernel denies op
```

This includes operations performed:

- directly by Brush;
- by a carried uutils command;
- by a host command admitted by Brush;
- by a child or grandchild spawned by an admitted command;
- through path traversal, symlinks, hard links, `/proc/self/fd`, or shell redirection.

The claim is valid only under the explicit assumptions in the next section.

---

## 3. Trusted assumptions

A security proof that hides its assumptions is a decorative napkin. Record these assumptions in code, CI, and disclosure output.

### A1. Kernel enforcement is real

The running Linux kernel must support the required Landlock ABI and must fully enforce the requested rules. The implementation must use hard-requirement compatibility for security tests and must treat partial or unavailable enforcement as an error for restricted filesystem axes.

Minimum expectations:

- Landlock is enabled;
- read restrictions are supported;
- write restrictions include rename/refer and truncate semantics;
- `restrict_self` succeeds;
- `no_new_privs` is active;
- the reported restriction status is fully enforced.

### A2. The sandbox is applied before untrusted work

No Brush script, carried-command code, plugin code, shell expansion, or user-controlled callback may run before the sandbox is active.

### A3. No out-of-scope file descriptors are inherited

Landlock does not retroactively revoke access through file descriptors opened before sandboxing. The worker must not inherit a readable or writable descriptor for an out-of-scope object.

Only these descriptors should cross the boundary:

- stdin, stdout, and stderr, as explicitly configured;
- a narrowly scoped control channel;
- any descriptor explicitly declared in the tool envelope.

All other descriptors must be close-on-exec or explicitly closed.

### A4. Mount and user-namespace escapes are closed

Agent Bridle's `SECURITY.md` correctly identifies bind-mount re-pointing as a residual Landlock escape when unprivileged user namespaces are available.

A production proof requires one of these:

1. **Host prerequisite:** unprivileged user namespaces are disabled and verified before strong-mode execution; or
2. **Process backstop:** seccomp or a namespace launcher denies the mount and namespace syscall family needed to construct the escape; or
3. **Tier-2 jail:** a constructed mount namespace or micro-VM provides the filesystem view.

Until the process backstop lands, strong Linux execution must fail closed when the host prerequisite cannot be verified.

### A5. The worker is unprivileged

The sandboxed worker must not retain privileges that could defeat the boundary:

- no `CAP_SYS_ADMIN`;
- no ambient capabilities;
- no setuid transition;
- no writable path to a privileged helper;
- no ptrace authority over a more privileged process.

### A6. Kernel correctness is trusted

The claim assumes no exploitable kernel or Landlock vulnerability. This is the conventional trusted-computing-base boundary, not something Agent Bridle can prove in Rust.

---

## 4. Required architecture

## 4.1 Preferred design: a sandboxed Brush worker

Move full Brush execution into a dedicated worker process.

```text
Agent / MCP caller
        |
        v
Gate::authorize
        |
        v
BrushShellTool parent
        |
        | creates effective Caveats and worker request
        v
SandboxedWorker launcher
        |
        | applies Landlock / Seatbelt / AppContainer
        | fails closed if required fs axes are not enforceable
        v
Brush worker process
        |
        | parses and executes the shell program
        | L2 interceptor supplies admission, audit, and cancellation
        v
carried command re-exec
        |
        | inherits the worker's kernel sandbox
        v
uutils uumain and all descendants
```

The worker boundary provides several advantages:

- one OS sandbox covers Brush, carried commands, and descendants;
- Landlock inheritance becomes the proof-bearing mechanism;
- macOS can use a Seatbelt wrapper around the worker;
- Windows can eventually use an AppContainer process-creation boundary;
- a confined worker exits after the invocation, eliminating thread-pool contamination;
- file-descriptor hygiene is inspectable at one boundary;
- the L2 interceptor remains valuable for command admission and structured denials, but is no longer asked to police syscalls it cannot see.

## 4.2 Do not apply Landlock to a reusable Tokio worker thread

Landlock restrictions are inherited and monotonic for the confined thread. Applying them inside an arbitrary reusable `spawn_blocking` worker can poison that pool thread for later unrelated work.

Acceptable Linux alternatives are:

- a dedicated fresh `std::thread` that applies Landlock, runs Brush, and then exits; or
- the preferred dedicated worker process.

The process design is easier to generalize across operating systems and easier to audit.

## 4.3 Introduce a non-forgeable worker launch API

Do not expose a generic “skip `check_exec`” escape hatch.

The Brush worker executable is part of Agent Bridle's trusted runtime, not a user-selected command. Create a narrow API whose program and entry point cannot be supplied by the caller:

```rust
// Illustrative API, not drop-in code.
pub enum TrustedWorkerKind {
    Brush,
}

pub struct SandboxedWorker {
    kind: TrustedWorkerKind,
    policy: Arc<SandboxPolicy>,
}

impl SandboxedWorker {
    pub fn spawn_brush(
        request: BrushWorkerRequest,
        cx: &ToolContext,
    ) -> ToolResult<ConfinedWorker>;
}
```

Properties:

- the executable is always the canonical current Agent Bridle executable or a fixed helper;
- the hidden worker flag is fixed by the launcher;
- the worker refuses to start without a parent-created control descriptor and one-time nonce;
- the caller cannot substitute an arbitrary program;
- the worker receives the effective caveats, not the original broader grant;
- the sandbox is applied before the worker parses the shell command;
- the worker returns the achieved `SandboxKind` and per-axis enforcement report.

This is an internal runtime transition, not an `exec` grant to the model.

## 4.4 Pass policy over a trusted control channel

Do not rely on a shell-modifiable environment variable for the authoritative policy.

Recommended control channel:

1. parent creates a pipe or socket pair;
2. parent serializes a versioned `BrushWorkerRequest`;
3. request includes:
   - effective caveats;
   - normalized working directory;
   - shell program;
   - explicit environment;
   - output limits;
   - timeout/cancellation token identifier;
   - nonce;
4. child receives the descriptor at a fixed inherited FD;
5. child verifies framing, version, nonce, and request length;
6. child closes the control FD after reading;
7. child applies the sandbox before parsing or executing the shell text.

For Linux, a sealed `memfd` is also acceptable, but a pipe keeps the design portable.

## 4.5 Make filesystem restriction fail closed

The rule must be mechanical:

```text
if fs_read is restricted and achieved fs_read enforcement is not Kernel:
    refuse

if fs_write is restricted and achieved fs_write enforcement is not Kernel:
    refuse
```

Do not let an “advisory mode” silently run a command when the caller supplied a restrictive filesystem grant. Advisory execution may be a separate, explicitly selected posture, but it must not be mistaken for satisfaction of the grant.

This should reuse the existing `confinement_unenforceable` and `EnforcementReport` logic rather than creating a second strength calculation.

## 4.6 Report the sandbox that actually governed the worker

`BrushShellTool` currently constructs an envelope with `SandboxKind::None`. After this change:

- the parent must use the worker's achieved sandbox kind;
- `fs_read` and `fs_write` must report `Kernel` only when the worker was actually confined;
- a refused run must say which axis could not be enforced;
- the envelope should include a policy digest and useful evidence fields.

Suggested disclosure:

```json
{
  "engine": "brush",
  "worker_boundary": "process",
  "sandbox_kind": "landlock",
  "enforcement": {
    "fs_read": "kernel",
    "fs_write": "kernel",
    "exec": "interceptor",
    "net": "advisory"
  },
  "landlock_abi": 4,
  "no_new_privs": true,
  "policy_digest": "blake3:...",
  "carried_commands": ["cat", "ls", "echo"]
}
```

Do not upgrade Linux `exec` to `Kernel` merely because direct `execve` is restricted. ADR 0011 and ADR 0013 correctly reserve that claim for a minimal rootfs or equivalent program-identity boundary.

---

## 5. Code changes

The exact names may move, but the responsibilities should land in these areas.

### `agent-bridle-tool-shell/src/brush_shell.rs`

Change the invocation path so `BrushShellTool::invoke` launches a sandboxed worker rather than running `run_in_brush` on an ordinary blocking pool thread.

Required changes:

- construct a `BrushWorkerRequest`;
- launch the worker with the effective `ToolContext`;
- collect structured output, denials, timeout state, and disclosure;
- refuse restricted filesystem grants when no L3 backend is available;
- stop returning `SandboxKind::None` for a successfully kernel-confined run;
- preserve the L2 `CaveatInterceptor` inside the worker.

### `agent-bridle-tool-shell/src/coreutils_dispatch.rs`

Preserve the bundled-command dispatch path. The carried command's re-exec will inherit the worker sandbox.

Add defensive checks:

- dispatcher must run only in a process already marked as a trusted Brush worker, or in the existing dedicated dispatch test host;
- untrusted direct invocation of `--invoke-bundled` must not synthesize authority;
- the dispatcher must not widen environment, cwd, descriptors, or policy;
- unknown bundled names fail closed;
- command identity used for L2 admission remains the logical carried command name, not merely the host executable path.

### `agent-bridle-core/src/spawn.rs`

Factor the existing confinement sequence into a reusable internal primitive for trusted workers:

```text
probe backend
derive honest report
check strength/fail-closed rules
prepare wrapper if needed
spawn fresh confinement thread
apply thread-confining backend
spawn fixed worker
return achieved kind and evidence
```

Do not duplicate this logic in the shell crate. A duplicated sandbox funnel will eventually disagree with `ConfinedCommand`, which is precisely the kind of split-brain security boundary Agent Bridle is designed to prevent.

### `agent-bridle-core/src/sandbox.rs`

Return machine-checkable evidence from sandbox application, rather than only `()`.

Illustrative shape:

```rust
pub struct RestrictionEvidence {
    pub kind: SandboxKind,
    pub report: EnforcementReport,
    pub landlock_abi: Option<u32>,
    pub no_new_privs: bool,
    pub fully_enforced: bool,
    pub policy_digest: String,
}
```

The envelope may expose a redacted subset, while tests assert the complete internal evidence.

### New worker entry point

Add a private or hidden dispatch mode such as:

```text
--agent-bridle-worker brush --control-fd N --nonce HEX
```

It must:

1. validate the control channel;
2. clear unexpected environment;
3. close unexpected descriptors;
4. set resource limits;
5. apply or verify sandbox inheritance;
6. parse and execute the request;
7. emit a framed response;
8. exit.

A hidden command-line flag is not itself an authority boundary. The control descriptor, nonce, fixed executable, and parent-applied sandbox are the boundary.

---

## 6. The adversarial test suite

A security proof needs several kinds of evidence. One happy-path `cat` test is not enough.

## 6.1 Keystone regression test

Create two sibling directories:

```text
scratch/
  allowed/
    readable.txt
  denied/
    secret.txt
```

Grant:

```text
fs_read  = only[scratch/allowed]
fs_write = only[scratch/allowed]
exec     = only[cat]
```

Run through the actual public `BrushShellTool` path:

```sh
cat scratch/denied/secret.txt
```

Assertions:

- nonzero exit or tool refusal;
- secret contents never appear in stdout or stderr;
- envelope reports `fs_read = kernel`;
- envelope reports an actual L3 sandbox;
- the carried command was used, with host `PATH` scrubbed;
- no denial depends only on `before_open`, because the uutils child performs the open;
- the same binary can successfully read `scratch/allowed/readable.txt`.

This test should be red before the L3 worker change and green afterward.

## 6.2 Read escape matrix

Run every available read-capable carried command against an out-of-scope canary.

Initial commands:

```text
cat
ls
head
tail
wc
stat
sha256sum
base64
sort
```

Cases:

- absolute denied path;
- `../` traversal;
- symlink inside allowed root pointing to denied file;
- symlinked parent directory;
- hard link where the test platform permits creation;
- `/proc/self/fd/N` referring to a denied file;
- command substitution: `x=$(cat denied); printf '%s' "$x"`;
- pipeline: `cat denied | wc -c`;
- input redirection: `wc -c < denied`;
- subshell: `(cat denied)`;
- background child followed by `wait`;
- grandchild launched by an admitted wrapper in weak exec mode.

Every case must fail without leaking the canary bytes.

## 6.3 Write escape matrix

Create a denied directory with a sentinel file and immutable expected metadata.

Test:

```text
cp
dd
touch
truncate
mkdir
mv
rm
ln
```

Operations:

- create outside `fs_write`;
- overwrite outside `fs_write`;
- append outside `fs_write`;
- truncate outside `fs_write`;
- rename from allowed into denied;
- rename from denied into allowed;
- unlink outside `fs_write`;
- create symlink or hard link across the boundary;
- output redirection: `echo x > denied/file`;
- append redirection: `echo x >> denied/file`;
- `tee denied/file`;
- temporary-file then rename;
- write through a symlink inside the allowed root.

Assertions:

- operation fails;
- denied tree content and metadata remain unchanged;
- allowed writes still succeed;
- envelope reports `fs_write = kernel`.

## 6.4 File-descriptor inheritance tests

This suite is mandatory because pre-opened descriptors can bypass path-open checks.

Parent setup:

1. open the denied canary before launching the worker;
2. deliberately clear `FD_CLOEXEC` in a negative fixture;
3. launch the worker;
4. attempt to read `/proc/self/fd/<n>` or read directly from the inherited descriptor.

Expected production behavior:

- the production launcher closes or prevents inheritance of the descriptor;
- a test-only intentionally leaky launcher demonstrates that the canary would otherwise be reachable;
- CI therefore proves the descriptor hygiene code is load-bearing rather than ornamental.

Also test:

- directory descriptors opened outside scope;
- `openat(dirfd, "secret", ...)`;
- writable inherited descriptors;
- Unix-domain-socket descriptor passing, if the worker has any IPC socket.

## 6.5 Namespace and mount escape tests

On a dedicated Linux security runner:

```sh
unshare -Urnm sh -c 'mount --bind denied allowed/repoint && cat allowed/repoint/secret'
```

Expected:

- strong-mode startup refuses because host user namespaces are enabled; or
- the syscall backstop denies `unshare`/`mount`; or
- the mount namespace is already constructed and cannot be re-pointed.

Test the relevant modern syscall family as applicable:

```text
unshare
clone
clone3
mount
umount2
move_mount
open_tree
fsopen
fsconfig
fsmount
mount_setattr
pivot_root
chroot
```

The exact filter should be architecture guarded and reviewed independently. A syscall denylist that can be bypassed through x32 or i386 numbering is not a proof.

## 6.6 Sandbox availability and downgrade tests

Test each failure mode:

- build without `linux-landlock`;
- kernel without Landlock;
- Landlock disabled at boot;
- ABI below required floor;
- `restrict_self` failure;
- partial enforcement;
- unsupported configured path;
- wrapper missing on macOS.

For restricted `fs_read` or `fs_write`, every case must refuse before untrusted execution.

No test may accept:

```text
requested restricted filesystem axis
+
sandbox_kind = None
+
command executed
```

## 6.7 Process-tree inheritance tests

Use a small test helper carried or built for CI:

```text
parent -> child -> grandchild -> open denied path
```

Each generation must fail. Record PIDs and confirm the same Landlock domain behavior is inherited.

Also verify that a permitted command cannot daemonize past the worker lifetime and continue operating:

- double fork;
- `setsid`;
- background process;
- inherited stdout/stderr closure;
- timeout cancellation.

The worker supervisor should terminate the entire process group or cgroup on timeout.

## 6.8 Canary non-disclosure test

Use a high-entropy canary string, not a common word.

After every denied attempt, search:

- stdout;
- stderr;
- structured denials;
- tracing output;
- audit logs;
- panic messages;
- temporary files;
- crash dumps.

A denial path that prints the first bytes of a secret is still a confidentiality failure.

## 6.9 Property-based path generation

Generate path forms that resolve toward a denied inode:

- repeated separators;
- `.` and `..`;
- deep symlink chains;
- relative paths from varying cwd;
- Unicode normalization variants where the filesystem permits them;
- deleted-but-open files;
- bind-mounted aliases in the hardened runner;
- paths crossing mount points.

The property is based on the final kernel object, not string-prefix matching.

---

## 7. Proof artifacts

Tests alone show examples. The merge should produce a compact proof bundle.

## 7.1 Static argument

Document the refinement chain:

```text
effective Caveats
    -> normalized sandbox policy
    -> Landlock handled rights
    -> allow rules
    -> fully enforced restriction
    -> inherited worker domain
    -> inherited descendant domains
```

For each arrow, identify:

- implementation function;
- invariant;
- unit or integration test;
- failure disposition.

## 7.2 Runtime evidence

For each invocation, retain or expose:

- policy digest;
- effective caveats digest;
- sandbox kind;
- per-axis enforcement report;
- Landlock ABI;
- full-enforcement status;
- `no_new_privs` status;
- worker identity;
- worker start before shell parse;
- closed-FD summary;
- namespace-hardening status;
- refusal reason when a requirement is unmet.

This makes “the sandbox was active” an inspectable claim.

## 7.3 CI environment attestation

The Linux security job should print and archive:

```sh
uname -a
cat /sys/kernel/security/lsm
sysctl kernel.unprivileged_userns_clone 2>/dev/null || true
sysctl user.max_user_namespaces 2>/dev/null || true
sysctl kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || true
```

Also run Agent Bridle's own Landlock capability probe and record the ABI/evidence.

Do not infer security from the distribution name alone.

---

## 8. Historical merge gates (unchecked 2026-07-23 snapshot)

The original specification required all of the following. The boxes are
deliberately preserved unchecked as historical input; use the 0.7.14 status
addendum above, not this snapshot, for current implementation and verification
claims.

### Architecture gates

- [ ] Brush executes inside a dedicated, sandboxed worker boundary.
- [ ] The L3 sandbox is active before parsing or running user shell text.
- [ ] Carried commands and every descendant inherit the same filesystem boundary.
- [ ] The implementation reuses the core confinement funnel instead of duplicating it.
- [ ] Restricted `fs_read` and `fs_write` fail closed when kernel enforcement is unavailable.
- [ ] Worker descriptors are explicitly controlled.
- [ ] Worker environment begins empty and is populated from an allowlist.
- [ ] Worker cwd is normalized and checked before launch.
- [ ] Timeout kills the full process tree.
- [ ] Linux namespace/mount escape is closed or strong mode refuses the host.

### Honesty gates

- [ ] `BrushShellTool` no longer reports `SandboxKind::None` after a kernel-confined success.
- [ ] Linux `exec` remains `Interceptor` unless a minimal-rootfs identity boundary is actually present.
- [ ] The envelope distinguishes command admission from syscall confinement.
- [ ] The carried-command list is disclosed.
- [ ] Sandbox downgrade or partial enforcement is visible and cannot satisfy a restricted filesystem grant.

### Test gates

- [ ] Keystone carried `cat` denied-read test passes.
- [ ] Read escape matrix passes.
- [ ] Write escape matrix passes.
- [ ] Symlink and traversal matrix passes.
- [ ] File-descriptor inheritance tests pass.
- [ ] Namespace/mount escape tests pass.
- [ ] Process-tree inheritance test passes.
- [ ] Canary never appears in output or logs.
- [ ] PATH-scrubbed tests prove the carried implementation was used.
- [ ] Tests run on a real Landlock-capable Linux host, not only a container configuration that masks the relevant syscalls.

---

## 9. Historical staged implementation plan

## Stage 0: pin the failure

Add the keystone carried-`cat` test immediately.

Expected pre-remediation result:

```text
L2 admits cat
carried child opens denied file
secret is readable unless some outer sandbox happens to exist
```

Do not mark this test ignored. Run it only in a dedicated security feature/job if necessary, but keep the red proof visible until the boundary lands.

## Stage 1: add `SandboxedWorker`

Factor the confinement launch sequence from `ConfinedCommand` into a reusable internal primitive for fixed Agent Bridle workers.

Land the worker with a trivial echo protocol first. Prove:

- sandbox kind is returned;
- restricted filesystem axis fails closed;
- no ambient environment leaks;
- no unexpected FD leaks;
- child and grandchild inherit confinement.

## Stage 2: move Brush into the worker

Move `run_in_brush` execution behind the worker protocol.

Preserve:

- command interceptor;
- denial sink;
- cancellation hook;
- output cap;
- timeout;
- explicit environment;
- restricted PATH behavior;
- removed `exec` builtin.

The worker must apply or inherit L3 before constructing the Brush shell.

## Stage 3: run carried dispatch under the inherited domain

Keep `--invoke-bundled`, but prove the re-exec remains inside the worker's sandbox.

Add tests for:

```text
cat allowed
cat denied
ls allowed
ls denied
redirection
pipeline
subshell
```

## Stage 4: close host escapes

Implement one of:

- verified disabled unprivileged user namespaces for strong mode;
- architecture-correct seccomp backstop;
- mount-namespace rootfs.

Until then, mark Linux strong mode unsupported on a host that fails the prerequisite check.

## Stage 5: broaden the carried set

Only after the conformance suite is green should more uutils features be enabled.

Each newly carried command must be classified:

- read-only data transformer;
- filesystem mutator;
- process launcher/wrapper;
- privilege/context changer;
- special-file/device operation.

Commands that launch another program, change root/security context, or create special files need additional review and generally should not enter the baseline pack.

---

## 10. What this proof does and does not establish

### Established when all gates pass

- An admitted carried command cannot read file contents outside `fs_read`.
- An admitted carried command cannot mutate filesystem objects outside `fs_write`.
- The same restriction applies to descendants.
- The system refuses rather than silently weakening restricted filesystem grants.
- The result envelope accurately records the governing boundary.

### Not established

- A granted Linux interpreter cannot run ungranted code already readable inside the sandbox.
- Linux `exec` has program-identity confinement.
- Hostname-based network allowlists are kernel enforced.
- The kernel is free from vulnerabilities.
- Allowed data cannot be intentionally printed to stdout.
- Resource exhaustion is prevented unless separate CPU, memory, PID, output, and wall-clock limits are enforced.
- A privileged or incorrectly hardened host cannot defeat the boundary.

Those are separate claims and should remain separate in Agent Bridle's enforcement report.

---

## 11. Historical recommended issue statement

> **Close the carried-command syscall gap by placing Brush and its process tree behind the existing L3 confinement boundary.**
>
> The current carried-coreutils path proves dispatch and L2 command admission, but a uutils child opens its own files outside Brush's `before_open` hook. Add a dedicated sandboxed Brush worker that is born under Landlock/Seatbelt/AppContainer, fails closed for restricted filesystem axes, controls inherited descriptors, and carries the achieved per-axis enforcement evidence into the tool envelope.
>
> Acceptance requires an end-to-end, PATH-scrubbed test in which carried `cat` may read an allowed canary and is kernel-denied from reading a sibling denied canary, plus symlink, redirection, inherited-FD, descendant, sandbox-downgrade, and namespace-remount adversarial tests.

---

## 12. Source notes

At the 2026-07-23 baseline, Agent Bridle identified the carried Brush engine as
L2 advisory and noted that an L3 backstop was a follow-up:

- `agent-bridle-tool-shell/src/brush_shell.rs`
- <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/agent-bridle-tool-shell/src/brush_shell.rs>

The carried coreutils shim re-executes the host executable and dispatches to a uutils `uumain`:

- `agent-bridle-tool-shell/src/coreutils_dispatch.rs`
- <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/agent-bridle-tool-shell/src/coreutils_dispatch.rs>

The core `ConfinedCommand` already applies Landlock on a fresh thread before spawn so the child and descendants inherit the domain:

- `agent-bridle-core/src/spawn.rs`
- <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/agent-bridle-core/src/spawn.rs>

The Landlock backend governs restricted filesystem reads and writes and documents the loader/interpreter limitation for the exec axis:

- `agent-bridle-core/src/sandbox.rs`
- <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/agent-bridle-core/src/sandbox.rs>

Agent Bridle's host-hardening requirements and residual Landlock escape analysis:

- `SECURITY.md`
- <https://github.com/Gilamonster-Foundation/agent-bridle/security>

Relevant design decisions:

- ADR 0011, Landlock exec-axis co-confinement  
  <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/docs/adr/0011-landlock-exec-axis-co-confinement.md>
- ADR 0012, fence strength derived from caveats  
  <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/docs/adr/0012-fence-strength-derived-from-caveats.md>
- ADR 0013, minimal-rootfs program identity  
  <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/docs/adr/0013-tier2-program-identity-minimal-rootfs.md>
- ADR 0019, sandboxed-host shell engine  
  <https://github.com/Gilamonster-Foundation/agent-bridle/blob/main/docs/adr/0019-sandboxed-host-shell-engine.md>

Landlock's kernel documentation explains that rules are inherited by future children and that files opened before sandboxing are not retroactively restricted:

- <https://docs.kernel.org/userspace-api/landlock.html>
- <https://man7.org/linux/man-pages/man7/landlock.7.html>
