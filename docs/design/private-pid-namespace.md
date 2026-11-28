# Private PID namespace for confined builds: design and staged plan (#3321)

**Status.** Staged. Section 8 lists nine pull requests; PR 1 (the topology,
behind the opt-in `SandboxPolicy::private_pid_namespace`) has landed. PR 2
builds the authenticated startup/teardown protocol; as built in PR 3, Linux
private argv builds set the switch. The Brush production route stays off
until PR 9. The owner took every decision in
section 5 as recommended. Line references are to the tree the design was
written against and drift as the stages land.

Grades used below: *measured-here* was run on the design host (Linux 6.8,
Landlock ABI 4) while writing this document; *measured-prior* was run for the
earlier process-view study of the same issue, whose probes are not in the
repository; *read* is from the source; *believed* is unverified and has a
measurement in section 9.

## 1. Summary

Confined builds get a private PID namespace by extending the one existing pre-exec hook (`vendor/agent-bridle-fdguard/src/build_isolation.rs::confine`), so both build routes get it through the single spawn funnel with no second spawn path. The hook's process (std's direct child) stays in the host PID namespace as a tiny **relay**; it forks an **init** (PID 1: non-dumpable, signal-default, fd-less, blocked on a lifeline pipe, `SIGCHLD` ignored so the kernel auto-reaps orphans) and then the **payload** (PID 2), which finishes the existing hook (private `/proc` mount, Landlock grant, seccomp, Landlock) and returns to std to exec the unmodified command. The relay is the payload's real parent, so it mirrors the exact wait status; killing the relay by any means kills init, and the kernel then kills every namespace member, including ones that changed session. `spawn()` returns when the payload execs, exactly as today, so timeouts start on time. `/proc` is mounted `subset=pid`, read-only, and granted by a Landlock rule bound to the new mount object. The one affected test passes because the product stops reading `/proc/stat` and derives boot time from `clock_gettime`. Peer authentication is re-founded on kernel credentials in host numbering, on the kernel's own `NSpid` translation, and on a pidfd for the one claim about a third process. Any failure before the fence is installed takes the existing fresh-child deny-all retry.

## 2. Evidence base

| Claim | Grade | Evidence |
|---|---|---|
| `unshare(CLONE_NEWPID)` leaves the caller's PID unchanged; next child is PID 1 | measured-prior | `pre_exec` probe: caller PID unchanged, forked child is PID 1 |
| Naive fork/wait in the hook makes `spawn()` block for the payload lifetime (251 ms vs 0 ms) | measured-prior | same probe; cause is the waiting half keeping std's CLOEXEC error pipe open |
| Sibling topology works: relay outside, init PID 1, payload **PID 2, ppid 0, own pgrp 2**; procfs mounted by PID 2 | measured-here | Probe A: `"pid": 2, "ppid": 0, "pgrp": 2, "mount": "ok"` |
| In-namespace SIGTERM/SIGSTOP/SIGKILL to a default-disposition init are dropped | measured-here | Probe A: all three `"sent"`, `"init_alive_after": true` |
| Orphans reparent to PID 1 | measured-here | Probe A: `"orphan_ppid": "1"` |
| Normal exit: relay kills init, waits for it, then exits; a setsid double-forked descendant is already dead when the harness reaps the relay; exit code propagates | measured-here | Probe A: `"daemon_dead_at_relay_reap": true, "relay_status": 7` |
| Relay SIGKILLed: descendant dies, but asynchronously (not yet dead at the instant the relay is reaped; about 1 ms) | measured-here | Probe A: `"daemon_dead_at_relay_reap": false, "daemon_dead_within_5s": true, "teardown_ms": 1.07` |
| `subset=pid` removes `/proc/stat` and the other globals | measured-here and measured-prior | Probe A `"proc_stat_exists": false`; earlier procfs mount-option probe |
| `subset=pid` still exposes the host mount table and host cgroup path through per-pid files | measured-here | Probe A: `"mountinfo_lines": 68` (host has 67), `"cgroup": "0::/us"` |
| Non-dumpable init: `/proc/1/environ` EACCES, but `cmdline` and `stat` readable; with `hidepid=2` `/proc/1` is ENOENT and unlisted | measured-here | Probe A, both modes |
| A dumpable parent's stdout **pipe can be reopened for writing** by its child through `/proc/<ppid>/fd/1`, and its `environ` read (no Landlock in probe) | measured-here | Probe B: `"worker_fd1_write": "opened", "worker_environ": "opened"`; non-dumpable parent: both `ERR13` |
| A non-dumpable process keeps access to its own `/proc/self/{fd,exe,stat,maps,status}` | measured-here | Probe B `worker_self_view` all `opened` |
| `/proc/stat` btime equals `floor(CLOCK_REALTIME - CLOCK_BOOTTIME)` | measured-here (one sample) | `{"btime_minus_clock_derived_s": -0.48}` |
| A process in a child user namespace may signal its same-uid parent outside it | measured-here | `{"parent_outside_userns": "signal_permitted"}` |
| Inside the namespace the peer credential of an outside process is 0; the outside sees the host PID | measured-prior | Unix-socket peer-credential probe across a PID namespace |
| Supervision only ever sends SIGSTOP/SIGKILL to groups; nothing sends SIGTERM | read | `newt-core/src/confined_exec.rs:530`, `vendor/agent-bridle-tool-shell/src/lib.rs:107`, `brush_shell.rs:694` |
| Callers consume `status.code()` / `success()` only | read | `executor.rs:267-268`, `supervisor.rs:54` |
| Host has `CONFIG_PROC_CHILDREN=y`, `CONFIG_CHECKPOINT_RESTORE=y`, `pid_max` 4194304 | measured-here | the running kernel's build configuration and `pid_max` |
| Landlock denies `mount(2)` to any restricted process (so nested inner builds already fall back) | read | comment and test name at `vendor/agent-bridle-core/src/spawn.rs:1353-1355` |
| Everything marked "believed" below | believed | each has a measurement in section 9 |

## 3. Topology

```
 HOST PID NAMESPACE                          |  BUILD PID NAMESPACE (also private user/net/mount ns)
                                             |
 harness (newt)                              |
   |  std::process::Command::spawn           |
   |  (existing funnel: ConfinedCommand::spawn_authorized,
   |   hooks: deny_inherited_fds, then build_isolation::confine)
   v                                         |
 RELAY  = std's Child, host pid C            |
   never execs; group leader (pgid C)        |
   after forking: fenced, caps dropped,      |
   all fds closed except lifeline write end  |
   wait4(P) -> kill(I) -> wait4(I) -> mirror |
   |                                         |
   |-- clone3 #1 --------------------------->|  INIT = pid 1 (host pid Hi), never execs
   |      lifeline pipe  (relay w -> init r) |    PDEATHSIG=SIGKILL, non-dumpable, all signals SIG_DFL,
   |      ready pipe     (init w -> relay r) |    SIGCHLD=SIG_IGN (kernel auto-reaps orphans),
   |                                         |    fenced, caps dropped, argv area zeroed,
   |                                         |    blocks in read(lifeline); EOF -> _exit
   |                                         |    exit of pid 1 => kernel SIGKILLs every member
   |                                         |
   '-- clone3 #2 --------------------------->|  PAYLOAD = pid 2, ppid 0 (host pid Hp)
                                             |    setpgid(0,0) if the relay is a group leader
                                             |    mount private /proc, add Landlock rule,
                                             |    seccomp, landlock_restrict_self, write mode line,
                                             |    return Ok -> std execs the ORIGINAL command
                                             |      argv route : newt-net-guard ... -- program
                                             |      Brush route: <exe> --agent-bridle-worker brush
                                             |    descendants pid 3..n; orphans reparent to pid 1
```

Causal edges, none of them a clock:

- std's CLOEXEC error pipe closes when the payload execs: `spawn()` returns (payload started).
- ready pipe: one byte from init means init is hardened and waiting; EOF means init died, which is a setup failure.
- lifeline pipe: EOF at init means the relay is gone. `PR_SET_PDEATHSIG` is the second, independent trigger.
- `wait4(P)` in the relay: the payload finished; `wait4(I)` in the relay: the namespace is empty.
- pidfd of init held by the harness (stage 2): readable means the namespace is empty, on the kill path.

## 4. The nine areas

### 4.1 Process topology

**Who forks.** The existing hook, in std's fork child. `std::process::Command` cannot place its direct child in a new PID namespace (`unshare(CLONE_NEWPID)` affects children only; `CLONE_NEWUSER` cannot be unshared in the multithreaded harness), so with the existing funnel a host-namespace process between the harness and PID 1 is unavoidable. That process is the relay. Keeping it is an asset: it is outside the namespace, so no build process can name, signal or inspect it.

**Why the payload is PID 2 and not a child of init.** If init were the payload's parent it would have to fork, exec, forward signals and report the wait status out of band (PID 1 cannot kill itself with a default-disposition signal, so it cannot mirror a signal death). With the payload as a sibling created by the relay:

- the relay is the payload's real parent and gets the true wait status with no status pipe;
- init has no logic: it never forks, execs, forwards or reports;
- the payload has ordinary signal semantics (it is not PID 1), ordinary process-group leadership, and the unmodified `Command` (program, argv[0], env, cwd, stdio) is exec'd by std exactly as today, so there is no command wrapping and no extra exec grant.

**What PID 1 is.** A forked, never-exec'd child of the hook, about 40 lines of raw syscalls in `build_isolation.rs`. Not a binary and not a re-exec entrypoint. Reasons:

- A re-exec'd init needs an executable the vendored funnel can always run. There is none: the current executable is only dispatch-capable when the embedder's `main` calls `maybe_dispatch` (`vendor/agent-bridle-tool-shell/src/coreutils_dispatch.rs:136`), which libtest harnesses do not; `newt-net-guard` exists only on the argv route and lives in newt, not the vendored crates. Supplying an init image would add embedder-facing API, a Landlock execute grant, two dispatch sites and an exec-failure protocol.
- Because init is a sibling with no duties, there is nothing a "real" init would do that this one does not.

It is kept auditable by being one function with a fixed sequence and no branches on untrusted input:

```
init_main(life_r, ready_w):                       // all raw syscalls, no allocation
  prctl(PR_SET_PDEATHSIG, SIGKILL)
  prctl(PR_SET_DUMPABLE, 0)
  prctl(PR_SET_NAME, "build-init")
  zero [arg_start, arg_end)                        // hide the harness command line (see 4.4)
  sigaction(1..=64, SIG_DFL); sigaction(SIGCHLD, SIG_IGN)
  seccomp(deny filter); landlock_restrict_self(fallback ruleset)
  capset(all zero)
  close every fd except life_r and ready_w
  write(ready_w, "R"); close(ready_w)
  loop: read(life_r) == 0 -> _exit(0); EINTR -> retry; other error -> _exit(1)
```

The cost of not exec'ing is stated as residual risk R1 and owner decision D1: the relay and init are copy-on-write images of the harness for the build's lifetime.

As built in PR 1: the argv zeroing is PR 8's (section 8); init also sets `RLIMIT_CORE` to 0 because it shares the relay's hardening routine; a hardening failure makes init exit before the ready byte, which the relay treats as a setup failure.

**Relay.**

```
relay_main(init, payload):                        // never returns
  prctl(PR_SET_DUMPABLE, 0); setrlimit(RLIMIT_CORE, 0)
  sigaction(1..=64, SIG_DFL)
  seccomp(deny filter); landlock_restrict_self(fallback ruleset); capset(all zero)
  close every fd except life_w                    // drops std's error pipe copy and stdio copies
  loop: wait4(payload): EINTR -> retry; stopped (ptrace stop) -> kill(payload, SIGKILL)
  close(life_w); kill(init, SIGKILL); wait4(init) // either trigger ends init; returns only when the namespace is empty
  exited(code)   -> _exit(code)
  signaled(sig)  -> kill(getpid(), sig); _exit(128 + sig)
```

As built in PR 1: if the relay cannot install its own fences it writes one line to stderr and exits 1, which ends the namespace and the payload with it. If init does not report ready, or the second clone fails, the relay-to-be closes the lifeline, kills and reaps init, and only then returns the error, so no namespace outlives the failed attempt.

**Hook order** (new steps marked NEW; everything else is the existing code at `build_isolation.rs:86-147`):

```
stdio terminal / socket refusal                    (existing, in the relay-to-be)
if private:
  namespace()            unshare(USER|NET|NS), maps, lo up            (existing)
  private_filesystems()  tmpfs /tmp, /dev/shm, devpts                 (existing)
  private_identity()     passwd / group                               (existing)
  same-object Git aliases attach after private filesystems/account files (existing)
  NEW unshare(CLONE_NEWPID)            // separate call so fault injection can target it
  NEW pipe2(lifeline), pipe2(ready), both O_CLOEXEC
  NEW clone3 -> init_main (child)      ; relay reads one ready byte
  NEW clone3 -> relay_main (parent)    ; child continues below as the payload
  NEW setpgid(0,0) if the relay is a group leader
  NEW mount proc + Landlock rule                                        (stage 8)
  any error above -> Err(RETRY_DENY_ALL)
seccomp(filter); landlock_restrict_self(ruleset); write(2, mode line); Ok(())   (existing, now in the payload)
```

**Both routes, one path.** Both routes reach `agent_bridle_fdguard::build_isolation::confine` through `LandlockSandbox::apply_rules` (`vendor/agent-bridle-core/src/sandbox.rs:2167-2186`), called from `ConfinedCommand::spawn_authorized` (`spawn.rs:1055-1061`). The argv route arrives from `ConstrainedExecutor::run_until_cancelled` (`newt-core/src/confined_exec/executor.rs:175-199`); the Brush route from `SandboxedWorker::spawn_supported` (`spawn.rs:1331-1358`), with the control socket as stdin. The Brush worker's `/proc/self/fd/N` exec operand (`spawn/worker_image.rs:74`) resolves through the new procfs because the payload is a member of the namespace (believed; M9).

**Async-signal-safety.** The hook runs in a fork child of a multithreaded process. Allowed and used: `unshare`, `mount`, `open`, `pipe2`, `read`, `write`, `close_range`, `prctl`, `rt_sigaction`, `setpgid`, `getpgid`, `wait4`, `kill`, `setrlimit`, `capset`, `_exit`, the Landlock syscalls, and `seccompiler::apply_filter` (already used there). Rules:

- Fork with a raw `clone3` (`exit_signal = SIGCHLD`, a local `#[repr(C)]` 88-byte struct), never `libc::fork` (atfork handlers, allocator locks) and never `SYS_clone` (argument order differs by architecture).
- Never `raise` (libc caches the thread id, stale after a raw clone); use `kill(getpid(), sig)`.
- No allocation, no formatting, no Rust stdio, no panics. Everything variable is prepared in `confine()` before fork: the argv address range, the option strings, the filters and rulesets.
- What a re-exec'd init could do that this cannot: allocate, log, parse. None of that is needed.

**The one assumption about std.** `spawn()` learns exec success from EOF on a CLOEXEC pipe held by the child. The relay closes its inherited copy, so `spawn()` returns when the payload execs. A grounding test pins this (PR 1): `spawn()` must return while the payload is still blocked on a pipe the test holds.

### 4.2 Startup protocol

- **Payload started** is `Command::spawn` returning `Ok`. Same event as today (the payload's `execve` succeeded), so `executor.rs:225` and `brush_shell.rs:562` start their deadlines at the same point.
- **Failure before start** has two shapes, both ending in the existing retry at `spawn.rs:1066-1072`:
  - failure in the relay before the second clone: the hook returns `RETRY_DENY_ALL`, std reports it, the relay exits, init sees lifeline EOF;
  - failure in the payload after the second clone (proc mount, grant): the payload returns `RETRY_DENY_ALL`, std in the payload writes the error and exits 1, the relay reaps it, kills and reaps init, exits; the harness's std reads the error and reaps the relay.
  In both, no program has run and the fresh child never unshares. Seccomp or Landlock install failure in the payload still refuses outright, as today.
- **Mode, trusted.** Carry the shared fallback flag, including preselected fallback, out as `ConfinedChild::build_isolation: Option<BuildIsolation>` (`Private` or `DenyAllFallback`) and the same on `SandboxedWorkerChild`. This is harness memory, not child output, and both authentication and the diagnostic use it (4.5, 4.6).
- **Stable handles — As built in PR 2.** Before spawn the harness creates a close-on-exec `SOCK_SEQPACKET` socketpair and enables `SO_PASSCRED` on its receiver. Both `clone3` calls use `CLONE_PIDFD`: the kernel creates each handle atomically with its process. Init and payload close the sender as their first post-clone syscall. The never-exec'd relay sends one fixed report (marker and host init/payload coordinates) and both pidfds in one raw `sendmsg(SCM_RIGHTS)`, **after the second clone and before the relay installs its fences**. It then closes the sender before dropping its copy of std's exec-status pipe. There is no payload exec gate and no post-spawn PID lookup.
- **Report validation.** After spawn the harness drops its captured sender and receives exactly one complete message, then requires EOF. Truncated bytes or control data, extra/missing descriptors, missing/duplicate/wrong credentials, a second message, or an open sender refuse the launch. Kernel `SCM_CREDENTIALS` must name the spawned relay and the harness's uid/gid. Each received descriptor must expose a `Pid` field in its own fdinfo. For unreaped handles the host PID must match the report and any `NSpid` list must begin with that PID and end in 1 (init) or 2 (payload). The harness never reopens a process by its reported number.
- **Reaped handles.** Linux 6.8 reports `Pid: -1` and `NSpid: -1` after reap and has no `PIDFD_GET_INFO` ioctl (measured). Such handles are accepted on creator-channel provenance and kernel sender credentials; their discarded per-process metadata cannot attest their individual role (R13). They remain pollable and already count as gone. The delayed-reader measurement delivered two valid handles and exit 7 in 10/10 fast-exit interleavings; a live role mismatch was refused. `BuildNamespace` owns both handles through every child conversion and retains the reported payload coordinate for later authentication stages.

### 4.3 Signals, timeout, cancellation

- **What the harness sends today** (read): SIGSTOP then SIGKILL to the child's process group, then `child.kill()` (`vendor/agent-bridle-tool-shell/src/lib.rs:107-115`), or `killpg(SIGKILL)` plus optional `cgroup.kill` (`executor.rs:234-259`). No SIGTERM anywhere.
- **Reach.** The relay is the group leader (std applies `process_group(0)` before hooks) and init stays in that group. Any of group SIGKILL, `child.kill()`, kill-on-drop (`spawn.rs:1861-1872`, `brush_shell.rs:638-643`) or an OOM kill of the relay ends the relay; init then dies by PDEATHSIG or lifeline EOF, and the kernel SIGKILLs every namespace member and blocks new forks. This covers setsid and double-fork descendants on both routes without cgroups. The STOP-then-KILL ordering that exists to stop a shell from running its next command is subsumed: namespace teardown kills all members and fails new forks.
- **cgroup path** (argv route): `cgroup.kill` hits the payload subtree; the relay sees the payload die by SIGKILL, tears down, and mirrors SIGKILL. `newt-net-guard` writes `std::process::id()` (now 2) to `cgroup.procs`; the kernel interprets it in the writer's namespace (believed; M7).
- **SIGTERM.** Not forwarded, because nothing sends it. A fatal signal delivered to the relay is a hard stop of the build. If a graceful path is ever wanted, the relay gets a ten-line forwarding handler; do not add it speculatively.
- **Init's dispositions.** Everything `SIG_DFL` except `SIGCHLD = SIG_IGN`. As PID 1 with default dispositions the kernel drops every signal sent from inside the namespace, including SIGKILL and SIGSTOP (measured-here). Only an ancestor's SIGKILL/SIGSTOP are delivered.
- **Orphan reaping.** `SIGCHLD = SIG_IGN` makes the kernel reap init's children, including adopted orphans, with no reaping loop (believed for adopted zombies; M3).
- **Exit status.** The relay exits with the payload's code or dies by the payload's signal, so `status.code()` and `success()` are what callers see today. Core-dumping signals do not dump the harness image (`RLIMIT_CORE = 0`, non-dumpable).
- **Precise completion.** Normal exit: the relay only exits after `wait4(init)`, so when the harness reaps the relay the namespace is already empty (measured-here). Kill path: teardown is asynchronous by about a millisecond (measured-here); `BuildNamespace::wait_gone(bound)` polls the init pidfd, which becomes readable after the kernel has killed and reaped every member (measured, including an uninterruptible FUSE read; M2b). As built in PR 2, timeout/cancellation/drop have a separate 500 ms cleanup allowance, since the execution budget is already exhausted on timeout. Expiry reports incomplete teardown (a member may still be blocked in kernel I/O), never successful completion. Managed execution uses its existing reap grace. A timer only bounds waiting; it does not establish completion.
- **Harness crash.** The relay is deliberately not tied to the harness with PDEATHSIG: that signal fires when the creating thread exits, and the funnel spawns from a throwaway thread (`spawn.rs:978`). A crashed harness leaves the build running, as today.

### 4.4 procfs

**Mount**, by the payload (a namespace member that still holds capabilities and is not yet Landlocked):

```
mount("proc", "/proc", "proc", MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC, "subset=pid")
grant_private_mount(ruleset, scratch_access & (READ_FILE|READ_DIR), c"/proc")
```

The grant reuses `grant_private_mount` (`build_isolation.rs:383`): an `O_PATH|O_NOFOLLOW` descriptor opened after mounting binds the new procfs instance's root, never the host procfs it covers. If reads are not a handled access the rule is skipped, as the identity files do. No `hidepid`: see D3.

**What becomes readable, and the verdict on each:**

| Object | Readable by a build process | Verdict |
|---|---|---|
| Any process outside the build | No (not in the namespace) | Required property; measured-prior and here |
| `/proc/stat`, `meminfo`, `cpuinfo`, `/proc/sys/*` (hostname, boot id) | No (`subset=pid`) | Kept out; nothing in the product needs them after PR 6 |
| Own `stat`, `status`, `maps`, `fd`, `limits`, `exe` | Yes | The point of the change |
| Own `mountinfo` / `mounts` | Yes | **Discloses the host mount table** (mount points, devices, network share sources) |
| Own `cgroup`, `cpuset` | Yes | **Discloses the host cgroup path** (user slice, session scope) |
| Own `status` fields `Cpus_allowed`, `Mems_allowed`, `ns/*` inode numbers, `loginuid`, `sessionid` | Yes | Low-value host facts |
| Same-uid, same-domain siblings in the same build: `stat`, `status`, `cmdline`, `environ`, `fd/*`, `maps` | Yes (kernel ptrace-read rules) | Accepted: one build invocation is one trust domain, and each invocation has its own namespace and procfs instance, so nothing crosses builds |
| Brush worker (PID 2, trusted): `environ`, `fd/*` | Yes unless prevented | Not accepted for the result channel; see below |
| Init (PID 1): `environ`, `fd`, `maps`, `mem`, `exe` | No (non-dumpable; the image's owning user namespace is the initial one, so the required capability is unobtainable) | Measured-here for `environ` |
| Init: `cmdline`, `stat`, `status`, `statm` | Yes | `cmdline` is zeroed by init; the memory-size fields disclose the harness's footprint (residual R2) |

**The Brush worker inside the namespace.** The worker stays dumpable. Making it non-dumpable would silently turn the carried child's parent-image check (`private_control.rs:545-548`, reached from `authenticate_carried_dispatch`) from a real comparison into the `Unknown` fallback, which is a relaxation. What needs protecting is specific:

- **stdout and stderr** carry the framed result stream and the mode line (`brush_shell.rs:604-612`). A pipe can be reopened through `/proc/<pid>/fd/N`; a socket cannot. Give the worker socketpairs for stdout and stderr. This is PR 7 and is needed regardless of PID namespaces if M0 confirms that Landlock does not block the reopen today.
- **`environ`** holds `AGENT_BRIDLE_WORKER_NONCE`. It is consumed once, before any child exists (`brush_worker.rs:104-108`), and the control descriptor is retired afterwards (`private_control.rs:156`). Reading it later gains nothing. Accepted and stated.
- **`mem`** is governed by ptrace-attach rules as today; the read-only mount additionally refuses write opens (measured-here: `ERR30`).

**Boot time for the one test.** Change where the product reads it. `pid_start_unix_nanos` (`newt-core/src/store/liveness.rs:188-213`) keeps reading `/proc/<pid>/stat` field 22 and replaces the `/proc/stat` read with `floor(CLOCK_REALTIME - CLOCK_BOOTTIME)`, reading REALTIME first so any skew biases earlier, the fail-closed direction the existing comment requires. This is the same quantity the kernel prints as `btime` (measured-here, one sample; M11). Rejected alternatives: granting exactly `/proc/stat` (not expressible: `subset=pid` removes the file, and without `subset` the whole mount is granted, plus the file carries host activity counters); a synthetic file (new mechanism for a value two syscalls return).

**Residual disclosure, plainly.** After this change a build can read its own host mount table and cgroup path, every per-pid file of its own processes, and the size of the harness's address space through init's `stat`. It cannot read any other process on the host, nor the global files. `subset=pid` is not a secrecy boundary for boot time, uptime, CPU count, memory total or hostname: `clock_gettime(CLOCK_BOOTTIME)`, `sysinfo(2)`, `sched_getaffinity(2)` and `uname(2)` return them to any process today. The design does not claim otherwise.

### 4.5 Namespace-aware peer authentication

Rules, then each check.

- **R-A.** The outside never acts on a PID reported by the inside. Every PID the host uses comes from kernel credentials (`SCM_CREDENTIALS`, host numbering) or from the host's own procfs about such a PID.
- **R-B.** A self-reported PID is compared with the kernel's translation of the kernel-attested PID: the last field of `NSpid:` in the host's `/proc/<K>/status`. With no namespace that field is `K`, so the check is today's equality. One code path, no mode branch.
- **R-C.** A claim about a third process crosses the boundary as a pidfd, not a number. The host reads `Pid:` from its own `/proc/self/fdinfo/<fd>`, which is that process's PID in the host procfs's namespace.
- **R-D.** The inside cannot name outside processes. What the kernel does attest to it is "the sender is not in my PID namespace" (credential PID 0) and the uid. Every process a build can run is inside the namespace, so that predicate excludes exactly the adversary. Image and parentage of an outside peer are verified by the outside, which can see both sides.
- **R-E.** Host PIDs are used only while pinned: by un-reaped ancestry (`C` is the harness's un-reaped child; `Hp` has parent `C`), by a held pidfd, and by the existing start-time double read.

| # | Check today | Why it breaks | Re-founded as |
|---|---|---|---|
| 1 | Harness accepts the worker hello if `reported_pid == child.id()`, read with plain `read_exact` (`spawn.rs:204-214`) | reported is 2; `child.id()` is the relay | Enable `SO_PASSCRED` on the harness end before spawn and read the hello with `recvmsg`. Let `K` be the credential PID. Require credential uid equal to ours and `nspid_innermost(K) == reported`. If `build_isolation == Private`: `ppid(K) == child.id()`, `NSpid(K)` has exactly two levels, and `K == BuildNamespace::payload_pid`. Otherwise: `K == child.id()`. In both: `/proc/K/exe` device and inode equal the worker image. Strictly stronger than today (today has no kernel credential on this frame). |
| 2 | Worker requires `SO_PEERCRED.pid == getppid() == SCM pid`, uid, and the peer image (`private_control.rs:526-559`) | all three PIDs are 0; `/proc/0/exe` is ENOENT, a hard error | If `getppid() != 0`: unchanged. If `getppid() == 0` (kernel: my parent is in an ancestor PID namespace): require `SO_PEERCRED.pid == 0` and frame `SCM pid == 0` in both snapshots, uid equal, and treat the image as not attestable from inside (explicit branch, not a general ENOENT fallback). Image and parentage are covered by check 1; the nonce and challenge are unchanged. |
| 3 | Broker server requires `peer_pid == declared_pid` (`broker_transport.rs:314`) | declared is namespace-local | `nspid_innermost(peer_pid) == declared_pid` (R-B). |
| 4 | Broker server requires the worker's requests to come from `worker_pid = confined.child.id()` (`brush_shell.rs:402`, `broker_runtime.rs:185-187`) | `child.id()` is the relay | `worker_pid` is the authenticated `K` from check 1, bound into the runtime after authentication; requests before binding are refused. |
| 5 | Broker client requires the server's self-declared PID to equal the kernel credential (`broker_transport.rs:150-154`) | credential is 0 inside | Accept `credential == declared`, or `credential == 0` when the client's `/proc/self/ns/pid` is not the initial namespace (`pid:[4026531836]`, the kernel's fixed initial inode). In the initial namespace a zero credential is refused. uid check unchanged. The existing comment already says endpoint delegation, not this PID, selects the server. |
| 6 | `Request::Spawn { invocation, pid }`: host looks up the reported `pid` in its own procfs and requires parent `== worker_pid` and the admitted image (`broker_runtime.rs:293-313`) | reported PID is namespace-local; in the host procfs it names an unrelated process | The worker opens a pidfd on its child (rustix `pidfd_open`) and attaches it to the request; `Body.endpoint` already exists in the content-addressed frame and is currently refused for requests (`broker_transport.rs:324`). The host reads `Pid:` from fdinfo (R-C), requires ppid `== worker_pid`, the admitted image, and `nspid_innermost == pid`, records the birth, and keeps the pidfd for the session. Works unchanged without a namespace. |
| 7 | Helper requests: `peer_identity(peer_pid)` then `parent_process_id == registered.process_id`, births, images (`broker_transport.rs:368-387`, `broker_runtime.rs:205-212`, `newt-core/src/native_git_broker.rs:839-843`) | nothing: every operand is host numbering once check 6 registers a host PID | Unchanged. |
| 8 | Carried coreutils (`authorize_carried_child`, `authenticate_carried_dispatch`) | nothing: both ends are inside the same namespace | Unchanged. With `/proc` readable the parent-side image check becomes a real comparison more often. |

**Parents that appear as 0.** Only check 2 and check 5 run inside and look outward; both are covered. Parent and ancestor checks about processes inside the namespace are made by the host in host numbering (checks 6 and 7), where parents are real PIDs.

**PID reuse.** R-E, plus:

- the host never stores or compares a namespace-local PID;
- a build cannot steer namespace-local PID allocation: `ns_last_pid` is under `/proc/sys` (absent with `subset=pid`, and the mount is read-only) and `clone3(set_tid)` needs a capability in the namespace's owning user namespace, which exec'd build code does not hold (believed; M13);
- a build that nests its own user and PID namespaces produces `NSpid` with three levels; check 1 requires exactly two, and checks 6 and 7 use host numbering only.

**Stored owner identities** (`StoredOwner`, `liveness.rs:10-24`). Inside a private build `current_host_boot()` already degrades to `("localhost", "")` because the two `/proc/sys` files are absent, so an in-build claim never equals a host claim and is judged by heartbeat freshness, the existing cross-host path. The unsound case is two builds: both are `("localhost", "", n)` with colliding namespace-local `n`, and `claim` would treat the other build's row as its own (`liveness.rs:58-59`). Fix: when `/proc/self/ns/pid` is readable and is not the initial namespace, append it to the boot coordinate (`"<boot_id>;pid:[<inode>]"`). No schema change, host rows are unchanged, and a row from another namespace takes the heartbeat path. An inode reused by a later namespace can only collide with a row whose owner is dead, because a live owner keeps its namespace, and therefore its inode, alive. No new identity or hash is minted: a PID and a namespace inode are foreign kernel locators, and the broker frames stay on the existing canonical `MerkleNode` envelope.

### 4.6 Fallback and capability detection

- **Detection is by attempt**, as for every other private mechanism: there is no pre-probe. `unshare(CLONE_NEWPID)` failing (no `CONFIG_PID_NS`, `max_pid_namespaces`, a seccomp-filtered container), `clone3` failing, init not reporting ready, the proc mount being refused (a container that masks parts of `/proc`) or the Landlock rule failing all return `RETRY_DENY_ALL`.
- **One private mode.** There is no "private without PID namespace" tier (D2). Either the whole private view is installed or the fresh child runs deny-all.
- **No partial view reaches execution.** The payload returns `Ok` only after every step; every earlier exit discards all three processes; the retry child never unshares. Fault injection proves it at each new step (`--deny-build-pidns`, `--deny-build-proc`, section 8).
- **Disclosure.**
  - Trusted record: `ConfinedChild::build_isolation` / `SandboxedWorkerChild::build_isolation`. The harness should print the mode from this, not rediscover it from child stderr, which a payload can imitate on the argv route today.
  - Child-written line (kept; the existing tests match on substrings that stay): `PRIVATE` gains "private process IDs and /proc (this build's processes only)"; `FALLBACK` gains "host process IDs remain visible and signalable".
  - As built in PR 3, the argv mode line and permission prompt name private process IDs only. Private procfs is not implemented until PR 8. PID-enabled fallback explicitly names host process IDs as visible and signalable; switch-off messages stay unchanged.
  - Permission prompt: `build_isolation_description` (`newt-core/src/agentic/tools.rs:1385`) gains the same clause.
  - Enforcement report: `ChildNetworkPolicy::PrivateBuild` stays the mechanism tag (D5). The effective mode is execution evidence, as it is for the tmpfs, devpts and account-file mechanisms.
- **Nested builds** are unchanged: an inner hook under an outer Landlock domain fails at its first `mount` (before the new PID step) and takes the deny-all retry, exactly as today.

### 4.7 Interactions

- **Seccomp.** The production filters only match `socket` families and two tty ioctls (`sandbox.rs:2444-2486`) and are installed after all namespace work, so nothing new is filtered. The test harness's inherited fault filters match `unshare` and `mount`; the new steps are deliberately separate calls with distinguishable arguments (`unshare` with exactly `CLONE_NEWPID`; `mount` with flags exactly 15).
- **Private devpts and terminal refusal.** Unchanged and still first in the hook. Inside the namespace `getsid(0)` is 0 (the session leader is outside) where today it is the harness's session id; the payload is still a group leader, so `setsid()` still fails for it directly, as today.
- **Nested confinement.** Inner fences cannot mount, so they cannot create a full procfs to undo `subset=pid` (believed; M9c). With `/proc` now readable, an inner harness can read its own per-pid files; inner fenced children still cannot (their rulesets do not include the private mount).
- **Kill scope lost.** Today a build in a user namespace can signal any same-uid host process, including the harness (kernel behaviour measured-here; product baseline is M0). In the PID namespace no host process is nameable. `kill(0, ...)` and `kill(-1, ...)` reach only the build: the payload has its own process group, and init ignores signals from inside (believed for the group case; M5).
- **Ptrace scope.** Unchanged inside the build (same-domain siblings, subject to Yama). Host processes were already unreachable (user namespace). Init is non-dumpable; the relay is not addressable. `PTRACE_TRACEME` from the payload cannot enlist the relay as a tracer (it is in a sibling Landlock domain; believed, M6), and the relay kills a payload that reports a ptrace stop anyway.
- **Resource limits.** Two extra processes per build. No new limit is imposed; `RLIMIT_NPROC` and cgroup limits behave as before. The relay and init each account the harness's resident set (R1).
- **What a build newly can do.** Read its own `/proc` (and so run `ps`, and tests that read `/proc/self`), see its host mount table and cgroup path, inspect its own siblings.
- **What it loses.** Signalling and observing host processes; background processes surviving the command (they die with the namespace); stable large PIDs (PIDs are small and per-build); a visible parent for the first process (`getppid() == 0`).

### 4.8 Other platforms

- All topology code is in `build_isolation.rs`, already `#[cfg(target_os = "linux")] pub mod build_isolation` (`vendor/agent-bridle-fdguard/src/lib.rs:353`). macOS (Seatbelt) and Windows (AppContainer) never call it.
- `BuildIsolation` is a plain enum compiled everywhere, so call sites need no `cfg`; non-Linux always yields `None`.
- `nspid_innermost(pid)` and the pidfd attachment are `#[cfg(target_os = "linux")]`; the macOS arms of `read_broker_peer` and the snapshots are untouched, and macOS registers the child PID as today. Use the existing `let _ = ...;` stubs where a parameter is Linux-only.
- musl: raw `syscall(SYS_clone3, ...)`, `SYS_close_range` (as the existing guard does), `SYS_capset`, local structs for `clone_args` and the capability header, so nothing depends on glibc-only wrappers.
- Each PR runs `cargo clippy --workspace --all-targets -- -D warnings` on Linux, macOS and Windows, the `--no-default-features` lean configuration, and a `x86_64-unknown-linux-musl` build.

### 4.9 Upstream boundary

| Crate | What goes there | Why |
|---|---|---|
| `vendor/agent-bridle-fdguard` | The PID step, relay, init, proc mount and grant, and startup report socketpair in `build_isolation.rs`; namespace-safe rights receipt in `lib.rs` | It is the single unsafe seam; the PID setup stays together and descriptor receipt uses the existing leaf |
| `vendor/agent-bridle-core` | `BuildIsolation` and `BuildNamespace` on the child structs; kernel-credential hello in `TrustedWorkerControl::send_with_channel`; a descriptor-generic send and `receive_process_handle` beside `receive_control_endpoint` in `delegated_control.rs`; a pass-through in `sandbox.rs` | Funnel outputs and the control protocol already live here; core stays `forbid(unsafe_code)` (rustix plus the safe fdguard rights receiver; see PR 5 As built) |
| `vendor/agent-bridle-tool-shell` | Check 2 in `private_control.rs`; checks 3 and 5 in `broker_transport.rs`; checks 4 and 6 in `broker_runtime.rs`; plumbing in `brush_shell.rs`; socket stdio for the worker | The checks live here today |
| `newt-core` | `liveness.rs` (boot time, namespace coordinate); the staging switch in `ExecRequest::sandbox_policy`; description text; probes in `tests/brush_build_pipeline.rs`; `docs/security/ocap-deviations.md` | Product and policy, not mechanism |

The authentication changes are generalisations that are no-ops without a namespace, so they can be upstreamed independently of the PID step as "namespace-agnostic peer identity". Each vendored change gets its paragraph in that crate's `NEWT_PATCHES.md`.

## 5. Owner decisions

All seven were taken as recommended.

| # | Decision | Recommendation (taken) |
|---|---|---|
| D1 | Never-exec'd relay and init (this design) versus an embedder-supplied exec'd supervisor image | Never-exec'd now. Revisit only if M12 shows material copy-on-write growth or OOM-victim selection on a large harness; the upgrade is one builder method and two dispatch arms. |
| D2 | One private mode (a PID or procfs failure means deny-all fallback) versus degrading to today's private mode without a PID namespace | One mode. A second tier is a partial view by another name. Revisit only if M10 shows CI containers lose private mode. |
| D3 | `hidepid=2` on the private procfs | Not now. It hides init completely (measured-here) but turns a cross-domain parent's `EACCES` into `ENOENT`, which would force a change to the image fallback in `private_control.rs`. Zero init's argv instead and accept R2. |
| D4 | Register the native process by pidfd attachment (a vendored protocol change) versus scanning `/proc/<worker>/task/*/children` | pidfd. It needs no optional kernel configuration and is the same code with and without a namespace. |
| D5 | Rename the serialized mechanism tag (`private-build`) because the mechanism grew | Keep it, as the PTY and account-file additions did. |
| D6 | Enable on the argv route first (PR 3) versus one switch for both routes at the end | Argv first: it needs no authentication work and soaks the topology on the simpler route. |
| D7 | Also unshare the cgroup namespace to hide the cgroup path | No, until the interaction with the executor's cgroup join under `nsdelegate` is measured. |

## 6. Threat model notes

**Against init.**
- Signal it from inside: dropped by the kernel for a default-disposition PID 1 (measured-here).
- Read or write its memory, environment or descriptors: it is non-dumpable and its image belongs to the initial user namespace, so the access check requires a capability there; it has closed every descriptor but the lifeline read end; Landlock ptrace scoping is a second barrier.
- Make it exit: only lifeline EOF or the relay's death does that. The only lifeline write end is in the relay; the payload's copy is close-on-exec and closed explicitly.
- Signal the relay to trigger that: the relay is outside the namespace and outside the payload's process group. Succeeding would only kill the attacker's own build.
- Exhaust it: it never forks or allocates; orphans are reaped by the kernel.
- Have the OOM killer take it: the namespace dies and the harness sees a SIGKILLed build. Fail closed.

**Against the startup channel.**
- Every pipe is created by trusted code before any untrusted code exists. Untrusted code begins at the payload's `execve`, at which point every pipe end in that process is closed (close-on-exec plus the existing sweep, `fdguard/src/lib.rs:151`).
- The report cannot be forged from inside (no descriptor), and the harness validates it against its own procfs anyway.
- The ready byte comes from init before the payload process exists.
- A failure cannot be turned into a private start: the payload returns `Ok` only after the last step, and the retry child takes no namespace path.

**Against authentication.**
- Forge the worker hello from a build process: its kernel credential is a host PID that is not `payload_pid` and not a child of the relay.
- Replay: per-frame challenge, unchanged.
- Lie about a PID: every reported PID is compared with the kernel's translation (R-B) or replaced by a pidfd (R-C).
- Register a decoy native process: the pidfd must resolve to a child of the authenticated worker with the admitted image.
- Speak to the worker before the harness does: the sender would need a zero credential, which means being outside the namespace.
- Impersonate the broker server to a helper: needs the delegated endpoint, which is fd-passed and not reopenable.
- Nest namespaces to confuse translation: three `NSpid` levels fail check 1; host-numbered checks ignore inner levels.
- Recycle a PID: R-E.

## 7. Residual risks

- **R1. Harness images.** The relay and init are copy-on-write copies of the harness for the build's lifetime. Extra memory is bounded by the harness pages rewritten during the build; both account the harness's resident set for OOM scoring. Secrets in those images are protected by kernel access checks, not by absence.
- **R2. Init is visible.** Its `stat`, `status` and `statm` disclose the harness's memory footprint; its command line is zeroed.
- **R3. Per-pid host facts.** Host mount table and cgroup path through the build's own files.
- **R4. Same-build siblings** can read each other's environment and descriptors (and the worker's environment, including the spent nonce).
- **R5. Not secrets anyway.** Boot time, uptime, CPU count, memory total, hostname remain available by syscall.
- **R6. Fallback mode** keeps today's host PID view and kill scope; this is now disclosed.
- **R7. Kill-path teardown** is asynchronous until the harness waits on the init pidfd; a harness crash leaves the build running.
- **R8. One assumption about std** (fork plus close-on-exec error pipe), pinned by a grounding test.
- **R9. Inside-to-outside attestation** is "not in my namespace" plus uid, nothing finer. Inherent.
- **R10. Environments that refuse a fresh procfs** (masked `/proc` in containers) lose the entire private mode under D2.
- **R11. Yama scope 0 hosts**: same-domain ptrace inside a build, including of the worker. Pre-existing.
- **R12. Another procfs mount** at a path other than `/proc` in the host's mount table would let a build learn its own host PIDs by `readlink`. No authority follows from that.
- **R13. Reaped-handle provenance.** A reaped pidfd no longer exposes per-process identity. Its role is attested by the harness-created channel, the kernel credentials of the never-exec'd creator, and the creator's fixed clone/transfer code, not by per-descriptor metadata. A defect in that trusted creator substituting another reaped pidfd cannot be detected from those discarded facts. Live exposed facts must still match.

## 8. Staged pull requests

Baseline, measured-prior at `4ff638ac`: `9927 tests run: 9919 passed (2 slow), 8 failed, 293 skipped`, eight named tests. The expectations below are derived, not measured. For every PR: the same failing names unless stated; passed and skipped counts rise by the tests that PR adds; any other name appearing or disappearing must be explained before merge. Sizes are estimates of changed lines including tests.

Each PR updates the relevant `NEWT_PATCHES.md` and, where behaviour changes, `docs/security/ocap-deviations.md` (the Build-only exception paragraph). Branch names `fix/3321-<slug>`; bodies carry "What this PR does", "Test plan", "Out of scope".

**PR 1. fdguard: private PID namespace topology, opt-in (about 500 lines).**
- Change: the PID step, relay and init in `build_isolation.rs`; `SandboxPolicy::private_pid_namespace` (serde default false, a staging switch removed in PR 9); `BuildIsolation` on `ConfinedChild` and `SandboxedWorkerChild`.
- Red-first unit tests (mocked, no processes): the relay's status decision as a pure function (exit code, signal, ptrace stop); `clone_args` layout is 88 bytes with `exit_signal` at offset 32; `BuildIsolation` from the shared fallback flag.
- Real-resource proof in `brush_build_pipeline.rs`, new probe `--build-pid-probe`:
  - argv funnel (`ConfinedCommand` with the switch): payload prints PID 2, ppid 0, group leader; `kill(<harness pid>, 0)` is ESRCH; marker `BUILD_PRIVATE_PIDNS_CONFIRMED`.
  - `spawn()` returns while the payload is still blocked on a pipe the test holds (pins R8, replaces the 251 ms measurement with a causal assertion).
  - Brush launch (`SandboxedWorker::brush()` with the switch): the worker starts as PID 2 under the same funnel; authentication is asserted to refuse with the existing "worker hello PID mismatch" (the seam PR 4 turns green); dropping the child kills everything.
  - Forced fallback `--deny-build-pidns` (seccomp `EACCES` on `unshare` with exactly `CLONE_NEWPID`): both funnels report `DenyAllFallback` and "deny-all fallback".
  - As built, two additions: `--deny-build-pidns-init` (seccomp `EACCES` on `capset`) fails init's hardening inside the new namespace and must fall back the same way; and on the argv funnel, killing only the spawned child ends a payload and a `setsid` descendant that are both still blocked on a pipe the test holds.
- Acceptance: unchanged eight; production is untouched (switch off).

**PR 2. Startup report and teardown handle (about 400 lines).**
- Change: credentialed report socketpair and creation-time pidfds; `BuildNamespace`; `wait_gone`; both supervisors call it after a kill (`executor.rs` kill path, `OwnedWorker::terminate`).
- Red-first: report parsing against injected kernel facts: incomplete/malformed messages, truncation, a second message, wrong/missing credentials or descriptors, non-pidfds and live PID/namespace-role mismatches all refuse; reaped handles are accepted on creator provenance.
- Real-resource: a setsid double-forked descendant is dead when the call returns, on timeout, cancellation and drop, on the argv funnel; payload exit code 7 and death by SIGSEGV and SIGKILL are observed unchanged; an orphaned zombie is reaped by init.
- Acceptance: unchanged eight.

**PR 3. Enable on the argv route (about 120 lines; D6) — As built.**
- Change: `ExecRequest::sandbox_policy` (`newt-core/src/confined_exec.rs:279`) sets the switch for private builds; `PRIVATE` text variant; `build_isolation_description`.
- Red-first: the pure policy test in `confined_exec/sandbox_policy.rs` gains the switch expectation.
- Real-resource: `--build-pid-probe` through `ConstrainedExecutor::run(build_tool_request(...))` in required and forced-fallback modes; every existing argv probe (loopback, scratch, PTY, identity) still passes in required mode.
- Acceptance: unchanged eight (the acceptance route is Brush, still off).

**PR 4. Bootstrap authentication (checks 1 and 2; about 400 lines).**
- Change: credentialed hello and payload identity in `spawn.rs`; the ancestor-launcher branch in `private_control.rs`; `brush_shell.rs` uses the authenticated PID.
- Red-first: pure predicate tests with injected facts: wrong parent, three `NSpid` levels, image mismatch, reported PID not equal to the translation, nonzero credential with `ppid == 0`, zero credential with `ppid != 0`; the flat case is byte-for-byte today's behaviour.
- Real-resource: the PR 1 Brush launch assertion flips: the worker authenticates under the PID namespace and runs a command with no broker.
- Acceptance: unchanged eight.

**As built in PR 4.** The fixed hello is read with credentialed recvmsg on
Linux, including fragmented delivery. The harness checks the live payload
pidfd on both sides of the procfs identity reads. Reaped payload handles fail
bootstrap authentication even though PR 2 can accept them for teardown.
SandboxedWorkerChild::authenticate caches the verified host coordinate and
challenge; authority and endpoint transfer retain the existing frame bytes.
Brush starts its broker only after authentication, bound to that coordinate.
The worker's ancestor branch requires zero parent, peer and frame-sender PIDs,
the same uid and stable snapshots; ordinary image checks remain unchanged.
The real probe writes and verifies a completed no-broker command result, so
output-event fragmentation cannot affect its assertion. The argv route remains
enabled from PR 3, the Brush production route remains off, and the eight
acceptance results remain unchanged.

**PR 5. Broker transport (checks 3 to 6; about 450 lines).**
- Change: `nspid_innermost`; client hello predicate; worker PID binding; pidfd attachment and `receive_process_handle`.
- Red-first: translation parser; request with an attachment that is not a pidfd, is not a child of the worker, or has the wrong image is refused; requests before binding are refused.
- Real-resource: a native Git commit through the broker with the switch on (the existing broker pipeline plus a `brush_build_pipeline.rs` case), and unchanged with it off.
- Acceptance: unchanged eight.

**As built in PR 5.** M8 confirmed namespace-local PID 2/3 translating to
host coordinates, the transferred child's pidfd `Pid:` field, and the initial
versus private namespace inode distinction. The existing canonical frame's
attachment bit carries the Linux child pidfd; no wire schema or hash changes.
The host checks the handle before and after parent/image/translation reads and
retains it with the session. A zero worker binding is refused before listening.

Two implementation details extend the plan. First, rustix 1.1.4's UCred cannot
represent ancestor PID zero. Draining and ignoring it still constructs that
invalid value. A measured overlapping-receiver case also rules out temporarily
disabling SO_PASSCRED. Linux descriptor receipt therefore uses a narrow raw
receiver in the existing fdguard unsafe seam, owning rights and skipping
credential bytes without changing socket options. Core remains unsafe-free;
credential authentication stays in the signed-PID private-control receiver.
Second, the shared peer snapshot used to reopen even the worker's parent image.
The real private commit measured EACCES on the non-dumpable relay's executable.
Only the bootstrap-bound translated worker omits that image lookup; own image,
birth, parent and parent birth are still checked twice. Flat worker snapshots
and all helper parent/image/birth checks remain intact.

The real Brush fixture uses the existing NativeGitBroker with an explicit
trusted CommitPolicy through its public constructor. It checks policy attribution,
HEAD advancement and protected branch refs with staging on/off and all forced
fallbacks. This is an embedding API, not a model-facing tool or production
switch change. No pre-exec code changes in this stage.

**PR 6. Liveness: boot time and namespace coordinate (newt-core only; about 120 lines; independent, can land first).**
- Change: `pid_start_unix_nanos` uses the two clocks; `current_host_boot` appends the PID-namespace link when it is not the initial one.
- Red-first: pure `boot_epoch(realtime, boottime)` and `owner_coordinate(boot_id, ns_link)` tests; two builds with the same local PID are not "ours" to each other.
- Grounding: the existing real test, plus an assertion that the clock-derived value equals `/proc/stat` btime where that file is readable.
- Acceptance: unchanged eight (the test still cannot read its own `stat`).

**PR 7. Worker result channel is not reopenable (about 150 lines; only if M0 confirms).**
- Change: socketpairs for the worker's stdout and stderr.
- Real-resource: a build command that opens `/proc/$PPID/fd/1` for writing fails, in both modes.
- Acceptance: unchanged eight.

**PR 8. Private procfs (about 350 lines).**
- Change: the mount and grant in the payload; init zeroes its argv area; mode text.
- Red-first: option string and access-mask selection; the argv range parser.
- Real-resource, new probe `--build-proc-probe` on both funnels: own `stat` readable; `/proc/stat`, `meminfo`, `cpuinfo`, `sys/kernel/hostname`, `sys/kernel/random/boot_id` absent; a host canary PID absent; init's `environ` denied and `cmdline` empty; a sibling visible; a write under `/proc` refused; `unshare` plus a fresh proc mount refused. Forced fallback `--deny-build-proc` (seccomp `EACCES` on `mount` with flags exactly 15): deny-all, `/proc` unreadable.
- Acceptance: unchanged eight (argv builds now have `/proc`; the acceptance route is Brush).

**PR 9. Enable on the Brush route, remove the switch (about 100 lines).**
- Change: private build always means PID namespace and procfs; the staging field and the two-variant text go away.
- Real-resource: every probe on both routes in required and forced-fallback modes.
- **This is the PR that makes `store::liveness_tests::pid_start_time_is_unix_nanos_on_the_same_scale_as_the_claim_clock` pass inside the confined build route.** It needs PR 6 (no `/proc/stat`) and PR 8 (own `stat` readable).
- Acceptance: seven failures, that one name gone, no new name. Also rerun that one test alone inside the confined build route and show `1 passed`.

If D6 is declined, PR 3 folds into PR 9.

## 9. Measurements before coding each stage

Each is a transient probe; none needs the product. Record the output in the PR body with its grade.

**Before PR 1**
- M0 (baseline, decides PR 7 and grounds 4.7): inside today's confined Brush build run `kill -0 <harness pid>; echo $?` and `sh -c 'echo x > /proc/$PPID/fd/1'; echo $?` and `stat -L /proc/$PPID/exe`. Expect: signal permitted; the reopen result is the open question.
- M1: Probe A (appendix) on the CI kernel as well as this host.
- M2a: Probe A with the `prctl(1, ...)` line removed (lifeline alone), then with the lifeline read replaced by `signal.pause()` (PDEATHSIG alone). The descendant must die in both.
- M2b: in Probe A's `kill_relay` branch, open a pidfd on init's host PID before the kill and assert the tombstone is already at EOF the moment the pidfd polls readable.
- M3: Probe A, but let the orphan exit, then assert `/proc/<orphan>` is gone (not state `Z`) once a pidfd on it is readable.
- M4: adapt the earlier `pre_exec` probe: raw `clone3` twice in `pre_exec`, the relay half closing all descriptors. Expect `spawn_ms=0` for a 250 ms payload; with the payload half returning `Err(ENONET)`, `spawn()` returns that error and does not hang. Build it for glibc and `x86_64-unknown-linux-musl`.
- M5: from the payload in Probe A, `os.kill(0, 0)`, `os.kill(-1, signal.SIGTERM)`, `os.killpg(os.getpgrp(), signal.SIGTERM)`; assert the relay survives each.
- M6: payload calls `ptrace(PTRACE_TRACEME)` with the relay in a separate Landlock domain. Expect EPERM.
- M7: inside the namespace write the local PID to a delegated `cgroup.procs`; from outside confirm the host PID is listed and `cgroup.kill` ends it.

Stage-1 results (measured on Linux 6.8, Landlock ABI 4; the CI kernel was not measured): M1 to M7 all matched. M2b held in 200 of 200 runs and M3 in 200 of 200. M4 printed `spawn_ms=0` with the payload reporting PID 2 and parent 0, and the error case returned errno 64 without hanging, for glibc and musl. M5: `kill(0,0)` and `killpg` were sent, `kill(-1,TERM)` returned ESRCH, and the relay survived; a control payload that skipped `setpgid(0,0)` killed the relay with `kill(0,TERM)`. M6: EPERM; in a control where the relay had not yet restricted itself the call succeeded and the relay killed the payload at its first ptrace stop. M7: the host listed the host PID for local PID 2 and `cgroup.kill` ended the build with status 137. One further check: with init's seccomp, Landlock and `capset` applied and the lifeline never read, `PR_SET_PDEATHSIG` alone still ended the namespace.

**Before PR 2**
- M2b again with a member stuck in a long uninterruptible syscall, to choose the bound's error text.

**Before PR 4 and PR 5**
- M8a: the peer-credential probe extended to print the outside's `grep NSpid /proc/<peer>/status` (expect two fields, the second equal to the declared PID).
- M8b: inside, `pidfd_open` a child and send it over the socket with `SCM_RIGHTS`; outside, `grep -E '^(Pid|NSpid)' /proc/self/fdinfo/<fd>`. Expect the host PID.
- M8c: inside, `readlink /proc/self/ns/pid`; expect a value other than `pid:[4026531836]`; on the host expect exactly that value.

**Before PR 6**
- M11: `python3 -c 'import time;r=time.clock_gettime(time.CLOCK_REALTIME);b=time.clock_gettime(time.CLOCK_BOOTTIME);print(int(r-b), [l for l in open("/proc/stat") if l.startswith("btime")][0])'` on at least three hosts, including one that has suspended.

**Before PR 8**
- M9a: Probe A plus a Landlock ruleset: mount in the payload, add a rule on an `O_PATH` descriptor of `/proc`, restrict, then read `/proc/self/stat` and exec through `/proc/self/fd/N`.
- M9b: Probe B under that ruleset (does Landlock change the pipe reopen result?).
- M9c: after restricting, `unshare -Urpf --mount-proc true`; expect failure.
- M10: Probe A inside the CI container image and under rootless Podman. Expect either success or a clean mount failure.
- M13: inside, `clone3` with `set_tid` and a write to `/proc/sys/kernel/ns_last_pid`; expect EPERM and ENOENT.

**Before PR 9**
- M12: start a long build with the switch on; sample `grep -E 'VmRSS|RssAnon' /proc/<relay>/status /proc/<init host pid>/status /proc/<harness>/status` and `cat /proc/<pid>/oom_score` at start and end while the harness works. Decide D1 from the growth.
- M14: `cargo nextest run` of a small crate inside the private build with `/proc` readable; compare thread counts and timings with the switch off (Rust's `available_parallelism` starts reading `/proc/self/cgroup` and `mountinfo`).

## Appendix: Probe A (measured-here; produced the section 2 rows)

Run as an unprivileged user; it writes nothing to disk.

```
timeout 60 python3 -c '
import os,sys,ctypes,signal,select,json,time
libc=ctypes.CDLL(None,use_errno=True)
NEWNS=0x20000;NEWUSER=0x10000000;NEWPID=0x20000000
def mount(src,tgt,fs,flags,data):
    if libc.mount(src,tgt,fs,ctypes.c_ulong(flags),data)!=0: raise OSError(ctypes.get_errno(),"mount")
def rd(p):
    try: return open(p,"rb").read()
    except OSError as e: return "ERR%d"%e.errno
def run(mode,ending):
    rr,rw=os.pipe(); tr,tw=os.pipe(); cr,cw=os.pipe()
    uid=os.getuid();gid=os.getgid()
    c=os.fork()
    if c==0:
        os.close(rr);os.close(tr);os.close(cw)
        os.setpgid(0,0)
        os.unshare(NEWUSER|NEWPID|NEWNS)
        open("/proc/self/uid_map","w").write("%d %d 1\n"%(uid,uid))
        open("/proc/self/setgroups","w").write("deny")
        open("/proc/self/gid_map","w").write("%d %d 1\n"%(gid,gid))
        mount(None,b"/",None,0x4000|0x40000,None)
        lr,lw=os.pipe(); gr,gw=os.pipe()
        i=os.fork()
        if i==0:
            for f in (lw,gr,rw,tw,cr): os.close(f)
            libc.prctl(1,int(signal.SIGKILL),0,0,0)
            libc.prctl(4,0,0,0,0)
            signal.signal(signal.SIGCHLD,signal.SIG_IGN)
            os.write(gw,b"R");os.close(gw)
            while True:
                if os.read(lr,1)==b"": os._exit(0)
        os.close(lr);os.close(gw)
        assert os.read(gr,1)==b"R"
        p=os.fork()
        if p==0:
            os.close(lw)
            os.setpgid(0,0)
            res={"pid":os.getpid(),"ppid":os.getppid(),"pgrp":os.getpgrp()}
            try:
                mount(b"proc",b"/proc",b"proc",1|2|4|8,mode); res["mount"]="ok"
            except OSError as e: res["mount"]="ERR%d"%e.errno
            res["proc_pids"]=sorted(x for x in os.listdir("/proc") if x.isdigit())
            res["proc1_cmdline"]=str(rd("/proc/1/cmdline"))[:12]
            res["proc1_environ"]=str(rd("/proc/1/environ"))[:8]
            res["proc1_stat"]=str(rd("/proc/1/stat"))[:8]
            res["proc_stat_exists"]=os.path.exists("/proc/stat")
            res["self_stat_ok"]=not isinstance(rd("/proc/self/stat"),str)
            res["nspid_self"]=[l for l in rd("/proc/self/status").decode().splitlines() if l.startswith("NSpid")]
            res["ns_pid"]=os.readlink("/proc/self/ns/pid")
            res["mountinfo_lines"]=len(rd("/proc/self/mountinfo").splitlines())
            res["cgroup"]=rd("/proc/self/cgroup").decode().strip()[:6]
            sig={}
            for s in (signal.SIGTERM,signal.SIGSTOP,signal.SIGKILL):
                try: os.kill(1,s); sig[s.name]="sent"
                except OSError as e: sig[s.name]="ERR%d"%e.errno
            try: os.kill(1,0); sig["init_alive_after"]=True
            except OSError: sig["init_alive_after"]=False
            res["kill_init"]=sig
            a,b=os.pipe()
            mid=os.fork()
            if mid==0:
                o=os.fork()
                if o==0:
                    os.write(b,b"%d\n"%os.getpid()); os.read(cr,1); os._exit(0)
                os._exit(0)
            os.waitpid(mid,0)
            orphan=int(os.read(a,16))
            st=rd("/proc/%d/stat"%orphan); res["orphan_ppid"]=st.decode().rsplit(")",1)[1].split()[1] if not isinstance(st,str) else st
            d=os.fork()
            if d==0:
                os.setsid()
                if os.fork()==0:
                    for f in (rw,cr,a,b): os.close(f)
                    while True: signal.pause()
                os._exit(0)
            os.waitpid(d,0)
            os.close(tw)
            os.write(rw,(json.dumps(res)+"\n").encode())
            x=os.read(cr,1)
            os._exit(7)
        os.close(tw);os.close(rw);os.close(cr)
        _,st=os.waitpid(p,0)
        os.kill(i,signal.SIGKILL); os.waitpid(i,0)
        os._exit(os.waitstatus_to_exitcode(st))
    os.close(rw);os.close(tw);os.close(cr)
    f=os.fdopen(rr); line=f.readline()
    res=json.loads(line) if line else {"error":"no report"}
    out={"mode":mode.decode(),"ending":ending,"inside":res}
    out["tomb_open_before"]=not select.select([tr],[],[],0)[0]
    if ending=="kill_relay":
        t=time.monotonic(); os.kill(c,signal.SIGKILL); _,st=os.waitpid(c,0)
        out["relay_status"]=os.waitstatus_to_exitcode(st)
        out["daemon_dead_at_relay_reap"]=bool(select.select([tr],[],[],0)[0])
        out["daemon_dead_within_5s"]=bool(select.select([tr],[],[],5)[0])
        out["teardown_ms"]=round((time.monotonic()-t)*1000,2)
    else:
        os.write(cw,b"xx"); _,st=os.waitpid(c,0)
        out["relay_status"]=os.waitstatus_to_exitcode(st)
        out["daemon_dead_at_relay_reap"]=bool(select.select([tr],[],[],0)[0])
    print(json.dumps(out,sort_keys=True))
for mode in (b"subset=pid",b"hidepid=2,subset=pid"):
    for ending in ("normal","kill_relay"):
        run(mode,ending)
'
```

Probe B is the same skeleton with the payload acting as a "worker": it replaces its fd 1 with a pipe, sets an environment variable, optionally calls `prctl(PR_SET_DUMPABLE, 0)`, forks a child, and the child attempts `open("/proc/<ppid>/fd/1", O_WRONLY)`, `open("/proc/<ppid>/environ")` and `open("/proc/<ppid>/mem", O_RDWR)`; the worker then opens its own `/proc/self/{fd/1,exe,stat,maps,status}`.
