# OCAP deviation register & ratchet

**Status:** Mechanism + living register (2026-06-16). The disciplined way to take
practical caveats for function while guaranteeing a guided, *enforced* path back to the
full object-capability vision. Seeded from the captured-shell threat model
([`../design/captured-shell-ocap.md`](../design/captured-shell-ocap.md) §4) and the
authority plane of [`../design/centaur-swarm-architecture.md`](../design/centaur-swarm-architecture.md).

## 1. The mechanism

We will ship before OCAP is fully enforced. The rule that lets us do so *without lying
about security* — and it reuses the `meet`-only Caveat algebra the threat model found
sound:

> **Effective authority = `meet`( the human's grant , what the currently-verified
> invariants can actually enforce ).**

- Every **dangerous capability** declares the OCAP **invariants** it requires.
- At runtime, newt **verifies** which invariants hold (a checker returns
  `verified | absent` + evidence).
- A capability is available **iff all** its invariants verify; otherwise it is
  **fail-closed OFF**, with an honest banner (`#73` pattern) — never silently insecure.
- A **deviation** is an invariant currently *absent*. While it is open, the system
  **caveats its own authority down** (refuses the dependent capabilities) to match what it
  can actually enforce. *"The harness only stamps what it can enforce."*
- **Closing** a deviation (building + verifying the invariant) removes a self-caveat and
  **unlocks** its capabilities — raising the ceiling **toward, never above** the human's
  grant.

This is a **ratchet**: invariants close, never silently re-open; the effective ceiling
rises monotonically; **zero deviations = full OCAP = full function.** A deviation is, quite
literally, an OCAP *caveat* the system applies to itself — so the compromise is bounded **by
construction** (the dangerous path is structurally unreachable), not by discipline.

## 2. Enforcement (the ratchet gate)

A deviation is only real if the system *enforces* the bound. Two enforcement points:

- **Runtime:** at the point a dangerous capability would be exercised, check its invariants;
  if any is absent, **deny** + banner. (e.g. `seed_live_credential()` refuses unless
  `verify_b1()` passes.)
- **CI — `just ocap-check`** (the analog of `cov-ci`): **fails** if
  (a) any dangerous capability is reachable without its required invariants verified;
  (b) a registered deviation lacks a compensating control or a closure criterion;
  (c) a previously-verified invariant regressed to absent (the ratchet — no silent
  backslide); or
  (d) a capability is enabled whose deviation is still open.
  This register is the source of truth; CI makes convergence enforceable, not aspirational.

## 3. Entry format

```
### <id>
- Invariant (ideal):        <the OCAP invariant being violated>
- Practical caveat (now):   <what we actually do>
- Residual:                 <honest severity, from the threat model>
- Disabled while open:      <the dangerous capabilities fail-closed OFF — the BOUND>
- Compensating controls:    <what else bounds the residual>
- Closure criterion:        <the concrete, runtime-verifiable condition that retires it>
- Ratchet guard:            <the test/assertion that it can't widen + stays gated>
- Status / owner / review-by
```

## 4. The register (seeded — open deviations)

| id | invariant | residual | disabled while open |
|---|---|---|---|
| `b1-os-isolation` | OS isolation + egress proxy | 🟡 GATED | live credentials, untrusted-remote voices (UNREACHABLE) |
| `local-deputy-egress` | no indirect egress via a host AF_UNIX / Windows named-pipe deputy | 🟠 ACTIVE (linux/windows) / 🟢 DENIED (macos) | (a confinement limitation on run_command/build/crew — not a gated capability) |
| `mach-xpc-ambient-deputy` | no indirect authority via an ambient host Mach/XPC service (macOS) | 🟠 ACTIVE (macos) | (a Seatbelt confinement limitation on run_command/build/crew — not a gated capability) |
| `windows-inheritable-handle-leak` | no ambient inherited OS object handles cross into an AppContainer child | 🟠 ACTIVE (Windows) | (a Windows confinement limitation on attacker-exec children — not a gated capability) |
| `windows-shelltool-env-inheritance` | confined Windows `run_command` children start from an explicit env allow-list, not the Newt parent's ambient env | 🟠 ACTIVE (Windows run_command) | (provider-bearing Newt processes must not rely on `run_command` env isolation until agent-bridle env-clear parity lands) |
| `unconfined-fallback-on-missing-backend` | attacker-exec refuses (never runs advisory) when the native fs/net backend is unavailable | 🟠 ACTIVE | (run_command advisory-fallback; fixed by a per-axis bridle strength floor) |
| `disclosure-gate-live-path` | tool-derived text value-filtered before it reaches the model, at every funnel | 🟢 closed | (a NEW model-ingress path added without routing through a funnel — guarded by the convergence audit) |
| `exec-behavior-bound` | exec bound to resolved-path behavior tier | 🟠 high | (bounded by `b1`) |
| `fs-canonical-containment` | object-bound fs (`openat2 RESOLVE_BENEATH`) | 🟢 closed (Linux) | (non-Linux lexical fallback) |
| `sod-proposer-not-worker` | cryptographic proposer ≠ worker | 🟠 high | auto-apply of any proposed policy |
| `mcp-under-leash` | every MCP call mediated at call time (witness-typed leash; authority = structural grant, never the tool name; no-persona ≠ unrestricted) | 🟢 closed | (credential broker → `b1`; per-call budget = follow-on) |
| `mcp-config-admission` | untrusted/disabled MCP config cannot spawn or dial | 🟢 closed (fail-closed) | admitting an untrusted server without out-of-repo approval |
| `acp-worker-fs-scope` | worker fs attenuated to the session workspace (caveat fence + object-bound) | 🟢 closed | (fence active; non-Linux keeps the lexical-prefix fallback) |
| `acp-worker-debug-authority` | no production worker dispatches under `Caveats::top()` without a signed operator key | 🟢 closed (compile-gated) | a production build reaching the unbounded-authority fallback |
| `config-plane-provenance` | an untrusted project `.newt/config.toml` cannot grant exec / endpoint (control-plane) authority | 🟢 closed (fail-closed) | (overlay stripped; ambient `./newt.toml` base control-plane strip = tracked follow-up) |
| `noninteractive-launch-policy` | `--non-interactive` changes interaction only; OCAP-off host exec is an explicit opt-in; authority resolved once, cannot widen from later env | 🟢 closed | (authority is a frozen `LaunchAuthority`; a new deep `env::var` authority read re-opens it — gated) |
| `p4-constrained-executor` | all attacker-influenced subprocess creation routes through one confined executor (kernel-backed, fail-closed) + host-shell child stripped of newt's control plane | 🟢 closed (migration+confinement+#8) | (a new `agent-exec-todo-p4` spawn site, or the yolo lane ceasing to strip the control plane — both ratchet-guarded); process-tree-cancel bounded by `b1` |
| `git-confused-deputy` | every harness `git` subprocess disarms the repo's config-based code-execution gadgets (`core.fsmonitor`, hooks, `diff.external`/`textconv`, pager, sshCommand) | 🟢 closed | (a raw `Command::new("git")` in the workspace re-introduced without `hardened_git`) |
| `posture-report-honesty` | every security-posture surface is DERIVED from the same `verify_*` invariants the gates enforce, never independent prose | 🟢 closed | (a posture surface that asserts a claim the verifiers don't back — a report/enforcement drift) |
| `platform-capability-ceiling` | an unsupported/unverified platform reports each guarantee as `unsupported` and REFUSES operations needing it — never a Linux-equivalent OCAP claim | 🟢 closed | (a non-Linux build silently claiming a kernel-backed guarantee it cannot provide) |

### b1-os-isolation
- **Invariant (ideal):** uid-namespace + Landlock fs + seccomp + default-deny netns + an
  egress proxy that is the *only* egress (DNS included).
- **Practical caveat (now):** the DIRECT-egress floor is **complete on every live attacker
  path** — `run_command` (agent-bridle 0.7.15 `ChildNetworkPolicy::DenyDirect`) and the
  `ConstrainedExecutor` callers (build_check / crew, `NetGrant::DenyAll`) both spawn under the
  seccomp `socket()`-family egress deny (TCP+UDP+DNS+raw) beneath the Landlock fs fence, **by
  default and fail-closed**. TWO gaps remain, both closed by the deferred netns / mediated-egress
  floor (#1599): (1) the CREDENTIAL-bearing broker (a seeded token presented to authorized outbound
  requests, the box unable to exfiltrate it) — gated here; and (2) INDIRECT egress via a host
  AF_UNIX deputy (the seccomp floor allows AF_UNIX and Landlock does not govern unix-socket
  `connect`) — a REACHABLE residual tracked as `local-deputy-egress`.
- **Residual:** 🟡 — a hostile child can no longer create a DIRECT off-box socket (denied on every
  path); it can still reach a local AF_UNIX deputy (`local-deputy-egress`);
  the residual is only the unbuilt broker that would make seeding a LIVE credential safe.
- **Disabled while open:** seeding a **live scoped credential** into the box; running a
  **genuinely-untrusted / foreign remote voice** that holds anything sensitive.
- **Compensating controls:** trusted-code-only tasks on trusted hosts; the credential stays
  *out of the box* (a broker presents it to outbound requests — the model never sees the
  value).
- **Closure criterion:** `verify_b1()` confirms the full stack present + fail-closed at
  session seed (kernel floor: Landlock-net 6.7; else a real egress proxy / netns).
- **Ratchet guard:** `seed_live_credential()` / `admit_untrusted_remote()` refuse unless
  `verify_b1()` passes; `ocap-check` asserts no caller bypasses; the verifier is re-run per
  session (no COW-cloned-pod skip).
- **Seccomp egress floor — real, PROVEN, now DEFAULT on every live path (b1 slices 1a/1b/2).** A
  seccomp `socket()`-family deny kernel-denies TCP+UDP+DNS+raw egress for a confined child — the
  UDP/DNS/raw gap Landlock (TCP-only) leaves open — beneath the Landlock fs fence. Two prior gaps
  are now closed: (a) the deployment coupling — the guard rides IN the shipped `newt` via the hidden
  `newt __net-guard` self-exec (slice 1a), so `NetGrant::DenyAll` no longer needs a separately
  packaged helper; and (b) the primary attacker path — `run_command` routes through agent-bridle's
  ShellTool, whose spawn newt cannot filter, so the floor is installed AT the spawn owner: published
  **agent-bridle 0.7.15** adds `ChildNetworkPolicy::DenyDirect`, which installs the same seccomp deny
  on bridle's confining thread; `run_command` opts every shell engine into it (slice 2). The
  `ConstrainedExecutor` callers (build_check / crew) default to `NetGrant::DenyAll` (slice 1b). So
  every live attacker-exec path now denies direct egress by default, fail-closed. `verify_network_
  confinement()` reports this floor **Verified** (grounded by `net_guard_executor.rs` + the
  run_command socket-denial proof). The credential-bearing `b1` (`verify_b1`, still `Absent`) is
  UNCHANGED and STILL gates `seed_live_credential` / `admit_untrusted_remote` — it additionally
  requires the mediated-egress broker (#1599), which is deliberately deferred.
- **0.8.0 disposition:** does not block v0.8.0 — being closed on the still-unreleased **0.8.0** line
  (OCAP enforcement-floor epic #749). The dangerous capabilities it gates (`seed_live_credential`,
  `admit_untrusted_remote`) are fail-closed OFF while it is open; **basic egress is now fully denied**
  (TCP+UDP+DNS+raw via seccomp — #1599's socket-level goal met on Linux; the netns/egress-proxy form
  remains for the credential-bearing floor). Bounded confinement-hardening follow-ons, each tracked and
  NONE blocking v0.8.0: **#1599** (mediated egress proxy / netns for the credential floor). **#1600
  (SafeSubset/confined-shell env inheritance) is CLOSED** (step-8.9) for Newt's env-seam input:
  `venv_env_map` is an allowlist — `ConfinedCommand::env_clear` + a narrow name passthrough
  (default `HOME`/`USER`) + explicit `~/.newt/shell-env/` file imports — never an ambient copy; a
  regression test proves a parent-only secret (non-allowlisted name) never reaches that env seam.
  Windows #1633 separately grounds the actual `run_command` child path and exposes a shared
  agent-bridle `ShellTool` Windows spawn residual: the child process still inherits the parent's
  ambient environment, including provider-shaped secrets, before Newt can observe it. That is tracked
  as ACTIVE under `windows-shelltool-env-inheritance`; no Newt-local compatibility shim is installed.
  **#1601 (inherited-fd hygiene) is CLOSED** (step-8.8):
  `newt-net-guard` calls `close_inherited_fds()` (`close_range(3, ~0)`) before exec, so every
  attacker-influenced child has all fds ≥ 3 closed — an inherited fd (a capability that bypasses
  pathname confinement) cannot cross the fence (`net_guard_fd_hygiene.rs`: a control proves the fd is
  otherwise inherited; the guarded child cannot read it). Remaining follow-ons are bounded by `b1`'s OS
  sandbox as the eventual backstop.
- **Unreachable-guard-symbols:** `seed_live_credential`, `admit_untrusted_remote` — the two
  capabilities this entry gates. `ocap-check` proves (`check_state_proofs`) that neither is CALLED
  anywhere outside its defining file + tests, so a future contributor who wires either (e.g. a
  `pa login`) trips CI until `verify_b1` flips `Verified`.
- **Status:** GATED — this entry is scoped to the **CREDENTIAL-bearing** floor only: seeding a live
  scoped credential / admitting an untrusted remote. Both are UNREACHABLE (guard-symbols above,
  machine-checked), fail-closed (`require(verify_b1())` refuses; `verify_b1` is `Absent`), and
  forward-ratcheted (the OCAP-DANGER/GATE gate is required at every site). They need the deliberately
  deferred mediated-egress broker (#1599); CLOSES when it lands + `verify_b1` flips.
  **Scope note (do not over-read):** the DIRECT off-box socket floor (AF_INET/AF_INET6/AF_PACKET) is
  complete + Verified (`verify_network_confinement`), but that is NOT the same as complete network
  confinement — a confined child can still reach a host AF_UNIX deputy (pathname **and** abstract;
  Landlock does not govern unix-socket `connect`, proven by `af_unix_deputy.rs`). That INDIRECT-egress
  residual is REACHABLE and is tracked as its own **ACTIVE** deviation `local-deputy-egress` (below),
  closed by the same deferred netns/#1599. owner: — · review-by: #1599 / epic #749

### local-deputy-egress
- **Invariant (ideal):** a confined attacker-exec child cannot cause network egress by ANY path —
  neither a direct off-box socket NOR an indirect relay through an ambient local deputy (a host
  process reachable over local IPC that performs network on the child's behalf).
- **Practical caveat (now):** the DIRECT half is enforced — the seccomp floor denies
  `socket(AF_INET/AF_INET6/AF_PACKET)` on every live path. The INDIRECT half is NOT: the floor
  deliberately allows `AF_UNIX`, and Landlock's `AccessFs` rights include no unix-socket-connect
  right, so a confined child can `connect()` to ANY host AF_UNIX socket it can address — BOTH pathname
  (outside the fs fence) AND abstract-namespace. Proven by `af_unix_deputy.rs` (control: an
  out-of-fence file read is EACCES-denied while both socket forms CONNECT) and, on the run_command
  route, `run_command_child_can_reach_an_af_unix_abstract_deputy`. On Windows the analogous local
  deputy is a host named pipe whose DACL admits `ALL APPLICATION PACKAGES`: direct AppContainer
  loopback is denied, but `appcontainer_named_pipe_deputy`
  (`newt-core/tests/windows_appcontainer_adversarial.rs`) proves the child can send a payload to the
  pipe and cause the host deputy to relay over loopback. If a network-relaying local deputy is
  reachable (e.g. an exposed container-runtime socket or pipe), indirect egress is possible.
- **Residual:** 🟠 REACHABLE — this is exactly why the public claim is "direct AF_INET/AF_INET6/
  AF_PACKET socket creation is denied", NOT "hostile code cannot exfiltrate over the network".
- **Disabled while open:** nothing — this is a confinement LIMITATION on the always-reachable
  `run_command` / build_check / crew paths, not a gated capability: there is no fail-closed toggle
  (the child runs with the incomplete confinement). It is therefore honestly **ACTIVE**, not
  GATED/BOUNDED — reachable, and not bounded by any CLOSED invariant (the fs fence does not govern
  socket connect, so it cannot bound which deputies the child reaches).
- **Compensating controls:** the DIRECT-egress floor stands (no self-opened off-box socket); the
  child is fs- and exec-fenced, limiting what it can DO with a deputy but not stopping it reaching
  one; a hardened host removes ambient network-relaying deputies (not a hard guarantee). FD-hygiene
  note: the run_command route's inherited-FD hygiene is CLOEXEC-based (std default + agent-bridle
  `set_cloexec`), NOT the explicit `close_range(3,~0)` the DenyAll `newt-net-guard` route performs
  (`run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close`) — a non-CLOEXEC network fd
  would be inherited, so "no pre-opened-fd bypass of the socket() filter" holds only because newt
  opens its real fds via std (CLOEXEC).
- **Closure criterion:** a network namespace (unprivileged netns — blocked by host policy on Ubuntu
  ≥ 23.10) or an equivalent that isolates BOTH the abstract unix namespace AND pathname reachability
  — the mediated-egress / netns floor of #1599 — plus the Windows named-pipe/local-IPC deputy
  surface, making the only egress an explicit broker capability.
- **Ratchet guard:** `af_unix_deputy.rs` and `appcontainer_named_pipe_deputy` PIN the current
  reachability, so a future fence that closes either platform's local-deputy path trips CI and
  forces this entry + the `verify_network_confinement` claim to widen honestly.
- **Platform scope (macOS — #1632):** `macos: DENIED`. Unlike Linux (seccomp allows AF_UNIX,
  Landlock does not govern unix-socket connect), macOS Seatbelt's `(deny network*)` governs the
  AF_UNIX `connect` **itself**, so a confined child cannot reach a pathname host deputy at all —
  a STRONGER guarantee than the fs fence alone. Proven on real `sandbox-exec` by
  `macos_seatbelt_adversarial::seatbelt_pathname_af_unix_deputy` (macOS 26.5.2). This does NOT
  close the entry: the **Linux** residual is unfixed, so the overall status stays OPEN. See
  `docs/security/platform/macos-evidence.md`.
- **Status:** OPEN — reachable + unbounded ON LINUX AND WINDOWS; the netns / mediated-egress floor
  (#1599) closes the Linux half, while Windows requires an equivalent local-IPC boundary. This is a
  genuine ACTIVE deviation on Linux and Windows; on macOS it is DENIED (above). A macOS result must
  not cosmetically close either residual. owner: — · review-by: #1599 / epic #749.

### mach-xpc-ambient-deputy
- **Invariant (ideal):** a confined child reaches NO ambient host service (Mach/XPC) that could act
  as a filesystem or network deputy on its behalf — the macOS analog of `local-deputy-egress`.
- **Practical caveat (now):** the generated Seatbelt SBPL profile starts from `(allow default)` and
  governs only `file-read*` / `file-write*` / `network*` / `process-exec*` (the axes Newt's `Caveats`
  express). **Mach service lookup (`bootstrap_look_up`, XPC discovery) stays AMBIENT** — a confined
  child can talk to host XPC/Mach services that a hostile deputy could expose (an XPC helper that
  performs fs or network on the caller's behalf would bypass the fs/net fences). Pinned by
  `macos_seatbelt_adversarial::seatbelt_mach_xpc_deputy_surface`, which asserts the profile does NOT
  yet deny mach-lookup, so containment is never over-reported.
- **Residual:** 🟠 REACHABLE (macOS only) — bounded in practice by what ambient XPC services exist on
  the host, but not by any Newt-enforced kernel rule.
- **Disabled while open:** nothing — a Seatbelt confinement LIMITATION on the always-reachable
  `run_command` / build_check / crew paths, not a gated capability (no fail-closed toggle; the child
  runs with the incomplete confinement). Honestly ACTIVE — reachable, not bounded by any CLOSED
  invariant (the fs/net fences do not govern mach-lookup).
- **Compensating controls:** the fs, exec, and direct-egress (`(deny network*)`) fences all stand,
  limiting what the child can DO with a deputy; a hardened host removes unnecessary ambient XPC
  services (not a hard guarantee); Newt tasks are trusted-code-on-trusted-host today.
- **Closure criterion:** the SBPL profile emits `(deny mach*)` (or an explicit mach-lookup
  allowlist) for a confined request, and a real-resource test proves an ambient XPC lookup is denied
  — mirroring the AF_UNIX closure on the network axis.
- **Ratchet guard:** `macos_seatbelt_adversarial::seatbelt_mach_xpc_deputy_surface` PINS the current
  ambient surface (`(allow default)`, no `(deny mach…)`), so a future profile that closes it trips
  the test and forces this entry to widen honestly.
- **Status:** OPEN — reachable + unbounded on macOS; Linux and Windows builds are unaffected (no Mach). This is a
  macOS-scoped ACTIVE deviation discovered by #1632. owner: — · review-by: #1599 / epic #749.

### windows-inheritable-handle-leak
- **Invariant (ideal):** an attacker-exec child receives only explicitly granted capabilities; arbitrary
  inheritable OS object handles already open in the Newt parent cannot cross the AppContainer launcher
  chain and remain usable inside the child.
- **Practical caveat (now):** Windows AppContainer confines path and network reach, but the current
  shared spawn proof does not establish that every inheritable parent handle is stripped before the
  AppContainer child starts. The real Windows proof `appcontainer_inheritable_handle_inheritance`
  (`newt-core/tests/windows_appcontainer_adversarial.rs`) deliberately creates an inheritable file
  handle in the parent, launches a PowerShell child through `ConstrainedExecutor`/AppContainer, and
  records whether the child can write through that raw handle value. The local Windows host produced
  `HANDLE-LEAK`; GitHub-hosted runner behavior may close this specific raw handle. Because one
  supported Windows host/launcher chain produced usable ambient object authority, this row remains
  ACTIVE until the shared spawn path proves arbitrary inheritable handles are blocked everywhere.
- **Residual:** ACTIVE (Windows) - host-sensitive but real. A path policy can deny opening a file while
  a pre-opened inheritable handle still grants access to that object on at least one supported Windows
  host.
- **Disabled while open:** nothing — this is a confinement limitation on attacker-exec children, not a
  gated feature. The child runs; the residual is bounded only by the discipline of not creating
  inheritable sensitive handles before the spawn.
- **Compensating controls:** most ordinary Rust/std handles are non-inheritable or CLOEXEC-equivalent
  by default; Newt does not intentionally grant sensitive inheritable handles to attacker-exec
  children. That is not a hard guarantee, so the row remains ACTIVE until the spawn path explicitly
  clears or constrains handle inheritance.
- **Closure criterion:** the Windows spawn path must prevent arbitrary inherited handles from crossing
  into AppContainer children, either by using a handle allow-list (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
  / `STARTUPINFOEX`) or by setting `bInheritHandles = false` through the launcher chain, with a
  positive control proving an intentionally allowed stdio/pipe handle still works when needed.
- **Ratchet guard:** `appcontainer_inheritable_handle_inheritance` always proves the child actually ran
  under AppContainer and records the observed inheritable-handle classification. When the shared fix
  lands, flip the test to require the marker write to fail on all Windows runners and keep a positive
  control for explicitly allowed handles.
- **Status:** OPEN — Windows-only ACTIVE residual; owner: — · review-by: Windows AppContainer handle
  hygiene follow-up.

### windows-shelltool-env-inheritance
- **Invariant (ideal):** every confined `run_command` child on Windows starts from an explicit
  environment allow-list: Newt's structured env seam plus any operator-granted shell env imports, never
  an ambient copy of the Newt parent process. Provider credentials such as `OPENAI_API_KEY` and
  `OPENAI_BASE_URL` must not cross unless explicitly granted for that child.
- **Practical caveat (now):** the actual Windows `run_command` route is
  `dispatch_bridled_shell` -> agent-bridle `ShellTool` -> `agent-bridle-aclaunch.exe`. In published
  agent-bridle 0.7.15, `agent-bridle-tool-shell` clears the ambient child env only under the Unix
  implementation; the Windows child env contract is still inherited. The route-level proof
  `run_command_windows_provider_env_inheritance_is_active`
  (`newt-core/src/agentic/tools_tests/helper_windows_appcontainer.rs`) launches through the real AppContainer backend and proves a
  parent-only `OPENAI_API_KEY` reaches the child. The separate `ConstrainedExecutor` seam does not have
  this leak: `appcontainer_child_does_not_inherit_provider_credentials`
  (`newt-core/tests/windows_appcontainer_adversarial.rs`) proves only explicitly granted env crosses
  there.
- **Residual:** 🟠 ACTIVE (Windows `run_command`) — reachable when Newt itself holds provider
  credentials in its process environment and then runs model-influenced `run_command` through the
  shared ShellTool path.
- **Disabled while open:** nothing automatic — this is a shared spawn-abstraction defect on an
  always-reachable attacker-exec route, not a gated capability. Operators who run Newt with live
  provider credentials should treat Windows `run_command` env isolation as unproven until the shared
  fix lands.
- **Compensating controls:** the AppContainer fs/net boundary still holds; this residual is env
  authority only. ConstrainedExecutor callers already strip ambient env. The correct fix belongs in
  agent-bridle's Windows ShellTool spawn path (or a shared ShellTool env policy), not in a Newt-local
  Windows wrapper around bridle.
- **Closure criterion:** consume an agent-bridle release whose Windows ShellTool starts from an empty
  or explicitly minimal environment, with a real Windows regression test proving a parent-only
  provider credential is absent while explicitly granted env still appears. Tracked upstream as
  Gilamonster-Foundation/agent-bridle#323.
- **Ratchet guard:** `run_command_windows_provider_env_inheritance_is_active` pins the current leak.
  When the shared fix lands, flip the test to require `EMPTY` and rename it back to a denial proof.
- **Status:** OPEN — Windows-only ACTIVE residual in the shared `ShellTool` route; owner:
  agent-bridle#323.

### unconfined-fallback-on-missing-backend
- **Invariant (ideal):** an attacker-exec route must, on EVERY supported platform, either enforce the
  requested fs/net authority with a real OS boundary OR REFUSE before hostile code runs — never
  silently fall back to advisory (host) execution because the native backend is unavailable.
- **Practical caveat (now):** the `ConstrainedExecutor` callers (build_check / crew) mint under a
  Kernel strength floor, so a missing fs/net backend REFUSES (`confinement_unenforceable`). The
  `run_command` route (`dispatch_bridled_shell`) dispatches at the DEFAULT (Advisory) floor. Where the
  native fs/net backend is present (Linux+Landlock, macOS+Seatbelt, Windows+AppContainer) the fence is
  kernel-enforced and this is confined. Windows feature-path update (#1633): with
  `windows-appcontainer` compiled in, hiding `agent-bridle-aclaunch.exe` from PATH now refuses before
  the hostile command runs (`run_command_windows_missing_launcher_refuses_not_host_fallback` and
  `appcontainer_missing_backend_refuses_not_host`). But where a RESTRICTED fs/net axis has NO backend
  at runtime or in the compiled feature set (old Linux w/o Landlock; `sandbox-exec` missing on macOS;
  Windows builds without the AppContainer backend), run_command can still collapse to ADVISORY — a
  compiled hostile child runs on the host (only the in-process L2 interceptor gates the brush engine's
  own exec decisions). A blanket Kernel floor is the WRONG fix: run_command legitimately restricts `exec`,
  which Landlock enforces only as `interceptor` (the `exec-behavior-bound` BOUNDED residual), so a
  blanket Kernel floor refuses every exec-restricted command even on Landlock (empirically verified in
  this pass).
- **Residual:** 🟠 REACHABLE on platforms/builds where the native fs/net backend is unavailable or not
  compiled in. The Windows `windows-appcontainer` feature path has a missing-launcher refusal proof,
  but that does not close the cross-platform/per-axis floor debt.
- **Disabled while open:** nothing — a confinement LIMITATION on the always-reachable run_command
  route, not a gated capability.
- **Compensating controls:** on the SUPPORTED platforms with the backend present (the normative case)
  run_command IS kernel-confined; Windows #1633 additionally proves missing-launcher refusal for the
  AppContainer feature path. The only documented unconfined route is the operator
  `--disable-ocap` / `--full-access` path, which is operator-frozen (`noninteractive-launch-policy`),
  never repo/model selectable.
- **Closure criterion:** a PER-AXIS strength floor at the agent-bridle boundary (fs/net = Kernel,
  exec = Interceptor-OK) so run_command REFUSES when the fs/net fence cannot be kernel-enforced while
  still tolerating the interceptor-level exec residual. Fix at the shared bridle boundary + consume a
  bugfix release; do NOT add newt-local duplicate sandbox logic.
- **Ratchet guard:** to land with the fix — a real-resource test that forces `backends.disable =
  ["landlock"]` (an available-but-disabled native backend) and asserts run_command REFUSES rather than
  running advisory.
- **Status:** OPEN — reachable on any config where the native fs/net backend is unavailable; the
  per-axis bridle floor closes it. owner: — · review-by: agent-bridle per-axis strength floor.

### disclosure-gate-live-path
- **Invariant (ideal):** *every* tool result passes a single disclosure filter before it is
  pushed into `messages` (one chokepoint at `mod.rs:1312`/`2025`).
- **Practical caveat (now):** **step-6.1a** wired the by-VALUE `ocap::DisclosureFilter` into the
  SINGLE live tool-result chokepoint — `maybe_offload_tool_result` (`agentic/mod.rs`), which all four
  backend loops call and nothing else to make a `{"role":"tool"}` content string, including the
  early-return tools (`run_command`/`lifecycle`/`prompt_read`/`artifact_read`) the offload/spill
  redaction never touched. Threaded via a new `ChatCtx.disclosure` (`None` = inert, bit-for-bit
  unchanged). **step-6.5 LANDED session-start registration + the summary path:** both live ChatCtx
  builders — the headless driver (`agentic/driver.rs`) and the interactive TUI (`newt-tui/chat.rs`) —
  now build a session filter via `ocap::session_disclosure_filter(api_key)` (registers the live
  provider bearer value; inert when absent) and pass `Some(&filter)`, so the tool-result chokepoint
  is LIVE; and the three backend loops' `final_summary_*` outputs are value-filtered through
  `redact_model_facing` before they leave the loop. Remaining gaps: (i) the next-turn
  **observation/compaction** memory still redacts shape-only (`redact_secrets`, 7 regexes), not by
  value; (ii) **streaming/chunked** deltas printed live and non-`api_key` secrets (MCP credential
  handles, brokered/temporary tokens) are not yet registered; (iii) `ChatCtx.disclosure` is still an
  `Option` — a *future* builder could pass `None` (the "no alternate path" guarantee wants the field
  made required).
- **Residual:** 🟢 closed. **step-6.6** routed the last funnels through the registered-secret value
  filter: the observation / compaction / spill memory path (`redact_secrets` now also applies the
  by-value filter) and, via a `scoped_session_disclosure` TLS backstop installed per driven turn, the
  tool-result chokepoint and the summary path too — so even a caller that forgot the explicit
  `&DisclosureFilter` param cannot place tool-derived text into model context unfiltered. Streaming /
  error deltas are covered transitively: with every INGRESS funnel filtered the model never RECEIVES
  a registered secret, so it cannot echo one into its streamed output (the summary path filters the
  final answer regardless). The one remaining risk — a FUTURE model-ingress path added without
  routing through a funnel — is a fresh-audit obligation, not a known open hole; it is the standing
  job of the convergence audit + would be caught by the guards below going stale.
- **Disabled while open:** seeding **any secret-bearing file the worker can `read_file`/
  `cat`** (until registration + convergence land).
- **Compensating controls:** keep secrets out of the box; the value-filter chokepoint (step-6.1a) is
  ready to redact by known value the moment B3 registers it — no longer only shape-matching.
  **step-6.2** hardened the by-value primitive itself to the full re-encoding matrix — base64
  (standard + url-safe, padded + unpadded), hex (lower + upper), percent/URL-encoding, the `\xXX` /
  `\uXXXX` string escapes, and **chunk-split** obfuscation (whitespace normalisation) — with `redact`
  now fail-closed (a split form that can't be excised inline withholds the whole text). So the moment
  registration lands, the live chokepoint catches every common exfil transform, not just raw/base64/
  hex.
- **Closure criterion:** all three disclosure paths share one **value** chokepoint; the session
  secret is registered at start; a canary seeded at session start never appears in the model-facing
  message stream in any encoding.
- **Ratchet guard:** `disclosure_chokepoint_redacts_registered_canary_in_every_encoding` (`agentic/mod.rs`,
  step-6.1a) — a registered canary embedded raw + base64 + hex in a tool result is absent from the
  chokepoint output in every encoding (`DisclosureFilter::leaks == false`), and the `None` path is
  byte-identical. The primitive's own matrix is guarded by `catches_base64url_reencoding`,
  `catches_base64_nopad_reencoding`, `catches_uppercase_hex`, `catches_percent_encoding`,
  `catches_string_escapes`, `catches_chunk_split_raw`, `catches_chunk_split_base64`,
  `redact_withholds_chunk_split`, and `redact_post_condition_holds_for_every_form` (`ocap.rs`,
  step-6.2). **step-6.5:** `session_filter_registers_a_real_provider_key` +
  `session_filter_ignores_trivial_or_absent_key` (`ocap.rs`) prove the live registration; the summary
  redaction is `redact_model_facing` at the three `final_summary_*` returns (`agentic/mod.rs`).
- **Status:** CLOSED — step-6.6. `verify_disclosure_gate()` now returns `Verified`: the session
  secret is registered at start (`session_disclosure_filter`, both live builders) and value-filtered
  at every model-ingress funnel (tool-result chokepoint, summary, memory/observation/compaction/spill)
  via the explicit param + the TLS backstop, proven by
  `no_model_ingress_funnel_leaks_a_registered_session_secret`,
  `redact_secrets_value_filters_a_registered_session_secret`,
  `session_tls_redacts_installed_secret_and_restores`, and the step-6.1a/6.2 chokepoint + matrix
  guards. Note: flipping this does NOT enable `seed_live_credential` — that still requires
  `verify_b1` (Absent). · owner: — · review-by: the convergence audit re-verifies no new bypass path.

### sod-proposer-not-worker
- **Invariant (ideal):** the policy-proposing surface is cryptographically a *different,
  more-trusted* identity than the confined worker; observe-then-propose is taint-aware.
- **Practical caveat (now):** `role` is a descriptive string; observe-then-propose would
  ingest the worker's own transcript as ground truth → the worker authors its own ceiling.
- **Residual:** 🟠 high — privilege escalation by self-proposal.
- **Disabled while open:** **auto-apply** of any proposed policy (every promotion requires a
  human approval bound to the lowered-Caveats hash) and observe-then-propose over a
  *credentialed* transcript.
- **Compensating controls:** human-in-the-loop approval (no auto-apply); meet-to-ceiling caps
  absolute authority regardless of proposal.
- **Closure criterion:** a distinct proposer key (`proposer_fp ≠ worker_fp`, verified) +
  taint-aware proposal that down-weights worker-controlled transcript.
- **Ratchet guard:** the promote path refuses if `proposer_fp == worker_fp`; `ocap-check`
  asserts no auto-apply path exists.
- **Unreachable-guard-symbols:** `auto_apply_policy` — `ocap-check` (`check_state_proofs`) proves it
  has no caller outside its defining file + tests, so wiring an auto-apply flow trips CI.
- **0.8.0 disposition:** DOES NOT block v0.8.0 — **follow-on**. `auto_apply_policy` is fail-closed
  OFF while this is open (every promotion needs a human approval bound to the lowered-`Caveats`
  hash), so no self-proposal privilege escalation is reachable; the distinctness half is already
  checkable. Not in the hostile-repo/model live-turn path. Tracked: this register entry (CI-gated).
- **Status:** GATED — the capability this gates (**auto-apply of a worker-proposed policy**) is
  **UNREACHABLE**: there is no swarm worker-proposal flow — proposal is operator-only, so
  `proposer == worker` never arises with a *distinct* worker to separate, and `auto_apply_policy`
  has no caller (grep-verifiable). It is **fail-closed** (every promotion requires a human approval
  bound to the lowered-`Caveats` hash; the promote path refuses `proposer_fp == worker_fp`) and the
  forward obligation is **machine-enforced** (`ocap-check` asserts no auto-apply path exists). Do
  NOT stand up a worker-proposal architecture merely to demonstrate SoD for a feature that does not
  exist; CLOSES if/when a distinct-proposer swarm flow is actually built. owner: — · review-by: when
  a worker-proposal flow is designed.

### fs-canonical-containment
- **Invariant (ideal):** the fs gate canonicalizes the target (resolving symlinks,
  e.g. via `openat2(RESOLVE_BENEATH)`) and contains it under the workspace root, so no
  path — symlink, `..`, or otherwise — escapes the fence.
- **Practical caveat (now):** `tui_permits_path` (`newt-core/src/agentic/tools.rs`)
  **lexically** normalizes the target and each root (collapsing `.`/`..`) and contains with
  a component-aware `Path::starts_with`. This closes `..` traversal (`/ws/../etc/passwd` →
  `/etc/passwd`, denied) and sibling-prefix escapes (`/ws-evil` denied against `/ws`) — both
  were reproduced on the host before the #502 review. It does NOT resolve symlinks: a symlink
  *inside* the workspace pointing out would still be read.
- **Residual:** 🟠 high — a symlink under the workspace that targets an outside path escapes
  the read/write fence (lexical normalization can't see through it). Planting one needs a
  write/exec, both separately gated.
- **Disabled while open:** seeding a shared filesystem across mutually-distrusting voices
  where one can plant a symlink the other follows; relying on the fence alone (no OS sandbox)
  for a genuinely-untrusted worker.
- **Compensating controls:** lexical containment (the #502 fix) blocks the `..`/sibling
  vectors; the crew/worker run in throwaway git worktrees; `b1`'s OS sandbox (Landlock fs) is
  the backstop that bounds the symlink residual once present. **The object-bound resolver now
  exists** — `newt_core::fs_cap::WorkspaceDir` (step-52.1) resolves every path *beneath* an
  `O_DIRECTORY` root fd with `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`, so the symlink /
  `..` / absolute escape is refused by the kernel at open time. It is proven and available; the
  residual persists only because the fs tool arms have not yet been *rewired* onto it.
- **Closure criterion:** the read arms, write arms, and write primitives resolve through
  `WorkspaceDir` (step-52.2/52.3) rather than `join` + a `&str→bool` predicate, so the escape is
  structurally unreachable. Note: the register's earlier "make `tui_permits_path` canonicalize,
  flip its assertion" plan is **superseded** — `tui_permits_path` is a lexical `&str→bool`
  predicate, and having it canonicalize would re-introduce the TOCTOU (check decoupled from
  open). The correct closure binds authority to the opened object; the proof is an object-level
  test (`fs_cap_object_bound.rs`, landed here) plus each arm's own contained-open test, not a
  predicate flip. `tui_permits_path_symlink_escape_is_the_known_residual` is retired when its
  arm moves to `WorkspaceDir`, not flipped in place.
- **Ratchet guard:** `newt-core/tests/fs_cap_object_bound.rs` (step-52.1, real-fs tier) drives
  real `..`, absolute, in-tree-relative-symlink-escape, and absolute-symlink-escape paths through
  `WorkspaceDir` and asserts denial, with an explicit contrast test proving the object resolver
  denies exactly what a lexical `starts_with` admits — neutering the resolve flags fails 5 of the
  8 (verified red→green). `read_file_symlink_under_workspace_escaping_is_denied` (`newt-core/src/agentic/tools_tests/execute_filesystem.rs`,
  step-52.2, real-fs tier) drives a confined `read_file` over a symlink-escape path and asserts the
  read is *denied* rather than exfiltrating the outside file (fails on the pre-rewire arm — verified
  red→green). `list_dir_symlink_under_workspace_escaping_is_denied` (`newt-core/src/agentic/tools_tests/execute_filesystem.rs`, step-52.3, real-fs
  tier) + `read_dir_lists_contained_entries` / `read_dir_denies_a_symlink_escape_directory`
  (`fs_cap_object_bound.rs`) drive a confined `list_dir` (and the underlying `WorkspaceDir::read_dir`)
  over a symlink-escape directory and assert it is *denied* rather than enumerated (verified
  red→green — neutering the resolve flags re-fails them). `write_file`'s escape residual is now
  proven closed by `physical_symlink_escape_write_is_denied_object_bound` (`newt-core/src/agentic/tools_tests/execute_file_artifacts.rs`, step-52.4) —
  the *flip* of the old `..._mutates_under_existing_policy...` test: a confined `write_file` through a
  symlink-escape path is denied and the outside file is left unchanged (verified red→green) — plus
  `create_dir_all_makes_nested_dirs_beneath` / `create_dir_all_denies_a_symlink_escape_component`
  (`fs_cap_object_bound.rs`) ground the object-bound `mkdir -p`. `edit_file_symlink_under_workspace_escaping_is_denied`
  (`newt-core/src/agentic/tools_tests/execute_filesystem.rs`, step-52.5) proves BOTH the read of `existing` (which could leak an outside file's head
  on a no-match) and the write are object-bound beneath the `fs_write` root — the escape is denied and
  the outside file is left unchanged (verified red→green). `delete_file_symlink_under_workspace_escaping_is_denied`
  (`newt-core/src/agentic/tools_tests/execute_filesystem.rs`, step-52.6) proves the removal is object-bound via `unlinkat` on the resolved parent — a
  symlink-escape delete is denied and the outside file survives (before the rewire `remove_file`
  followed the intermediate symlink and deleted outside); `unlink_removes_a_contained_file` /
  `unlink_denies_a_symlink_escape_parent` (`fs_cap_object_bound.rs`) ground the primitive. `find`'s
  recursive-read root is now object-bound to the workspace (replacing its canonicalize-`starts_with`
  TOCTOU; `find_refuses_root_outside_workspace` + `find_does_not_follow_symlinks_out_of_workspace`
  (`newt-core/src/agentic/tools_tests/execute_find.rs`) pin it). The `newt-tools` applier primitives (`apply_whole_files` / `fuzzy` / `diffy`) read AND
  write object-bound through one shared owner (`read_contained_opt` / `write_contained` +
  `WorkspaceDir::rename`), proven by `apply_whole_files_denies_symlink_escape_object_bound`
  (step-52.7). `tui_permits_path` is now documented as a lexical *pre-filter* (renamed
  `tui_permits_path_is_a_lexical_prefilter_not_the_fence`) — the object-bound arms are the fence.
- **Status:** CLOSED on Linux (step-52.7) — every read arm, write arm, and write primitive resolves
  through `WorkspaceDir` (`openat2 RESOLVE_BENEATH`), so a symlink / `..` / absolute escape is
  refused by the kernel at the open, not adjudicated by a normalized pathname; the lexical residual
  (#502→#522) is structurally unreachable. **Residual:** the non-Linux fallback keeps the lexical
  `std::fs` path (`openat2` is Linux-only) — bounded (CI + prod are Linux; the whole `fs_cap` module
  is `#[cfg(target_os = "linux")]`), documented, and a future hardening (fail-closed-for-untrusted
  on kernels without `openat2`, invariant #9). · review-by: with `b1` OS-sandbox work (#84)

### acp-worker-fs-scope
- **Invariant (ideal):** no production ACP/coder worker holds `fs_write = Scope::All`; every
  model-supplied write target is object-bound to the session workspace before any `join`, so a
  path with an absolute or `..` component cannot resolve outside the fence.
- **Practical caveat (now):** none for the workspace-escape vector. step-4.3 ACTIVATED the fence:
  `handle_prompt_coder` passes the session workspace to `caveats_for_dispatch`, so the dispatched
  authority is `fs_read`/`fs_write = Scope::only([workspace])`, not `Scope::All`. The coder's fs
  predicates gate by PREFIX containment (`newt_core::permits_path`) against that same workspace, so
  the fence permits the coder's own in-workspace writes and denies any `..`/absolute escape — and
  every write is additionally object-bound beneath the workspace fd at the two shared primitives
  (`apply_whole_files` / `apply_patch`, `newt-tools/src/patch.rs`).
- **Residual:** 🟢 closed — the `Scope::All` default is gone at the dispatch layer (attenuated to
  the workspace, step-4.3), the coarse caveat gate now denies escapes lexically (prefix), and the
  symlink-*under*-the-workspace escape is closed by the object-bound `WorkspaceDir`
  (`fs-canonical-containment`/#522 CLOSED on Linux, step-52.7). Two independent layers now contain a
  hostile model's write: the caveat fence AND the kernel `openat2 RESOLVE_BENEATH`.
- **Disabled while open:** n/a (closed). On non-Linux the object-bound layer degrades to the lexical
  prefix gate (`fs-canonical-containment`'s portability residual); the caveat fence itself is
  platform-independent.
- **Compensating controls:** the object-bound `WorkspaceDir` fence (#522, the kernel-enforced
  containment); the prefix caveat gate shared with the interactive tool sites
  (`newt_core::permits_path`); the crew/plan write path was already contained
  (`is_safe_worktree_path`); throwaway git worktrees; the `b1` OS sandbox backstop.
- **Closure criterion:** met — the dispatched worker's `fs_write`/`fs_read` are workspace-scoped
  (not `Scope::All`), the coder gate denies escapes by containment, and every write resolves
  object-bound beneath the workspace fd. Proven by executable tests at the unit, dispatch, and
  end-to-end tiers.
- **Ratchet guard:** `apply_under_workspace_fence_permits_inside_denies_escape`,
  `apply_whole_files_denies_atomically_on_partial_scope`, `apply_unified_diff_gated_on_workspace_fence`
  (`newt-coder`, coder gate); `caveats_for_dispatch_fences_fs_to_the_session_workspace`
  (`newt-acp-worker`, the fence is workspace-scoped); `coder_dispatch_under_fence_contains_workspace_escape`
  (`newt-acp-worker` integration — an operator dispatch emitting `../escape.rs` never creates the
  file and reports a dispatch error); plus the object-bound `apply_whole_files_denies_symlink_escape_object_bound`
  et al. (`newt-tools/src/patch.rs`, #522).
- **Status:** CLOSED — dispatch-site fence ACTIVATED (step-4.3) atop object-bound containment
  (#522/step-52.7); the coder fs predicates are prefix-aware and share `newt_core::permits_path`
  with the interactive gate · owner: — · review-by: if a non-workspace write grant (e.g. `--write`)
  is ever threaded into ACP dispatch.

### acp-worker-debug-authority
- **Invariant (ideal):** a production headless worker NEVER dispatches under `Caveats::top()`. The
  only two authorities it can hold are (a) an attenuated, signed operator identity, or (b) a
  fail-closed deny-all when no key resolves. The `--allow-no-key` debug escape hatch — which
  restores the pre-#94 `top()` dispatch via `WorkerIdentity::AllowNoKey` — must be **unreachable in
  a production build**, not merely discouraged by a scary flag name.
- **Practical caveat (now):** none in a production build — the fallback is compiled out. During
  local development the `allow-no-key` Cargo feature (off by default, in both `newt-acp-worker` and
  the `newt-agent` CLI) re-enables it so key-less iteration keeps working.
- **Mechanism:** the `top()` path is behind a compile-time feature, not a runtime flag. With the
  `allow-no-key` feature OFF (the default, hence every release build):
  `WorkerIdentity::resolve(path, allow_no_key)` **ignores** `allow_no_key` and propagates any
  key-load failure (the worker refuses to start), and `WorkerIdentity::AllowNoKey` — if constructed
  at all — yields `fail_closed_caveats()` (deny-all: `Scope::none()` on every axis,
  `CountBound::AtMost(0)`), never `top()`. Only when the feature is compiled in does `resolve` fall
  back to `AllowNoKey` and `caveats_for_dispatch` return `unbounded_debug_fallback()`. A runtime
  `--allow-no-key` on a production binary is therefore inert: it parses, but changes nothing.
- **Residual:** 🟢 closed for the production-authority vector. A developer who deliberately compiles
  `--features allow-no-key` still gets `top()` — that is the intended dev affordance, and such a
  build is not a production artifact.
- **Disabled while open:** n/a (closed). The dev feature must never be enabled in a released binary
  or a CI/bench image that runs foreign models.
- **Closure criterion:** met — `top()` is structurally unreachable without a compile-time opt-in,
  proven by executable tests in both feature configurations.
- **Ratchet guard:** `allow_no_key_authority_is_compile_gated` (`newt-acp-worker/src/identity.rs`,
  unit tier — asserts `AllowNoKey.caveats_for_dispatch(..)` is deny-all `fail_closed_caveats()` with
  `!permits_fs_read`/`!permits_fs_write` when the feature is off, and `top()` when on);
  `resolve_refuses_when_path_unresolved_without_allow_no_key` and (feature-off)
  `worker_allow_no_key_is_inert_in_production_build` (`newt-agent`
  `tests/worker_cli.rs`, real-subprocess tier — a production `newt worker --allow-no-key` with an
  unresolvable key exits with "refused to start" and never prints "unbounded debug authority").
  Flipping the feature re-enables the `top()` fallback (verified both directions).
- **Status:** CLOSED (compile-gated) — step-1.3 · owner: — · review-by: if a runtime approval path
  for key-less dispatch is ever designed (it should not be).

### mcp-config-admission
- **Invariant (ideal):** repository-controlled configuration cannot cause a process spawn or a
  network dial without an approval decision made *outside* the repository. An MCP server entry
  discovered from an untrusted origin (a cloned repo's `.mcp.json`, `~/.claude.json`, or a
  walked-up project `.newt/config.toml`, all stamped `McpTrust::Untrusted` by
  `newt_core::mcp::discover`), or any entry with `enabled = false`, is refused **before** any
  transport is constructed — never spawned, never dialled.
- **Practical caveat (now):** there is no interactive "approve this untrusted server" path yet, so
  an untrusted MCP entry is *always* refused (fail-closed) rather than promotable to admitted. The
  only servers that connect are `McpTrust::Trusted` ones — a newt-owned `~/.newt/config.toml` /
  `~/.newt/mcp.toml` the operator controls outside any cloned repo. This is the conservative end of
  the invariant (invariant #9: unsupported enforcement fails closed for untrusted origins), not a
  gap.
- **Mechanism:** one gate, `newt_core::mcp::admit(&McpServerEntry) -> Result<AdmittedServer<'_>,
  AdmissionDenied>`, decides `enabled && Trusted` at a single site and returns a witness
  (`AdmittedServer`, private field — unconstructable except by a successful `admit`). The four
  public transport entry points — `newt_mcp_client::{connect_stdio, connect_http}` and both
  planners (`McpToolset::connect` headless, `newt_tui::mcp::Mcp::connect` interactive) — take
  `&AdmittedServer`, so a `connect_*` on an un-admitted entry **does not compile**: the bug is
  unrepresentable, not merely unhit. **step-1.2** sealed the *lower-level* constructors too —
  `StdioTransport::spawn` and `HttpTransport::connect` now also require `&AdmittedServer` (not a bare
  `&McpServerEntry`), closing an adversarial-review finding that the witness was enforced only by
  convention at the two wrapper call sites; no in-crate or downstream caller can now reach a
  spawn/dial without the witness. Previously the headless planner connected *every* discovered
  entry (the interactive one already checked `enabled` but not trust), so a cloned repo shipping a
  `.mcp.json` could spawn an arbitrary subprocess on first agent turn — the closed vector.
- **Residual:** 🟢 closed for the spawn/dial vector. Remaining scope is *feature*, not exposure: no
  path yet promotes an untrusted server to admitted via an out-of-repo approval (would need a
  signed operator decision, per `sod-proposer-not-worker`'s spirit). Post-admission call-time
  leashing of the connected server is the separate, still-open `mcp-under-leash`.
- **Disabled while open:** admitting an untrusted server without an out-of-repo approval (there is
  no such path — untrusted stays refused).
- **Closure criterion:** met — the gate decides at one site, the witness type makes an un-admitted
  `connect_*` uncompilable, and both the decision and the wired planner behaviour are proven by
  executable tests.
- **Ratchet guard:** `admit_denies_untrusted_and_disabled_admits_trusted`
  (`newt-core/src/mcp.rs`, mocked unit tier) proves the gate *decides* deny for untrusted + disabled
  and admit for trusted; `headless_planner_never_spawns_an_untrusted_server`
  (`newt-mcp-client/tests/headless_admission_gate.rs`, real-resource tier, grounds the mocked gate)
  drives `McpToolset::connect` over an untrusted stdio entry whose command would `touch` a marker
  and asserts the marker never appears — proving the wired planner *acts* on the deny by never
  launching the process. Neutering the gate re-creates the marker (verified red→green).
- **Status:** CLOSED (fail-closed) — step-1.1 · owner: — · review-by: revisit if/when an
  out-of-repo untrusted-server approval path is designed.

### config-plane-provenance
- **Invariant (ideal):** repository-controlled configuration cannot grant executable or
  control-plane authority. A walked-up project `.newt/config.toml` (a cloned repo can ship one, so
  it is attacker-reachable, exactly like a `.mcp.json`) must not be able to run a command
  (`[[providers]]`, `[lifecycle]`), select the exec/shell backend (`[shell]`), or redirect the
  agent's inference/data endpoints (`[[backends]]`, `default_backend`, `[dgx]`, `[discovery]`) — via
  config alone. It may still pin benign, non-control-plane preferences.
- **Practical caveat (now):** none for the walked-up project-overlay vector. This generalizes the
  #1301 MCP-trust model (which already stamps a project overlay's `[[mcp_servers]]` `Untrusted`) to
  the rest of the control plane.
- **Mechanism:** control-plane authority is a **data table** — `CONTROL_PLANE_KEYS`
  (`newt-core/src/config.rs`) — and the raw `merge_toml` of the project overlay is replaced by
  `merge_project_overlay`, which `strip_control_plane`s the overlay at the `toml::Value` layer
  *before* `try_into::<Config>()`. A stripped key therefore fails closed to the trusted base's value
  (or the built-in default), never the attacker's. `mcp_servers` is deliberately left to its finer
  literal-only untrusted gate (`mark_project_mcp_untrusted`), not blanket-stripped.
- **Residual:** 🟢 closed for the walked-up `.newt/config.toml` overlay. Remaining scope is *feature*
  (a `newt config adopt` path so an operator can opt a repo's control-plane keys back in) and one
  sibling vector: the ambient `./newt.toml` **base** (`cd repo && newt`) — already MCP-downgraded by
  #1301 but its control-plane keys are not yet stripped. Tracked as the follow-up; cross-referenced
  here so it is not mistaken for closed.
- **Disabled while open:** n/a (closed for the overlay vector).
- **Closure criterion:** met — the project overlay's control-plane keys are stripped before
  deserialize, proven on the real `Config::resolve()` path.
- **Ratchet guard:** `untrusted_project_overlay_cannot_contribute_control_plane_keys`
  (`newt-core/src/config_tests/layering.rs`, pure unit — the strip at the merge seam) and
  `walked_up_project_config_cannot_grant_control_plane_authority`
  (`newt-core/tests/config_project_trust.rs`, real-resource `#[serial]` — plants a walked-up
  `.newt/config.toml` with an RCE provider + lifecycle command + host shell + exfil endpoint, runs
  real `Config::resolve()`, asserts every control-plane key is absent from the resolved config while
  a benign `[context]` preference survives; step-7.2 extended it to `[crews.*]` + `[loadouts.*]`).
  Neutering `strip_control_plane` re-admits them. step-7.2 (convergence audit) added `crews` +
  `loadouts` to `CONTROL_PLANE_KEYS`: a `crews[].test` is a `sh -c` verification command run on
  `newt crew`, and a bare `[loadouts.*]` passes validation, so an overlay could otherwise mint a
  (confined) command by declaring the sole auto-selected crew.
- **Status:** CLOSED (fail-closed) — step-7.1 + step-7.2 (crews/loadouts) · owner: — · review-by: when a `newt config adopt`
  path or the ambient-base control-plane strip is designed.

### noninteractive-launch-policy
- **Invariant (ideal):** launch authority is resolved once, explicitly, and cannot be widened by an
  ambient signal after the fact. `--non-interactive` changes INTERACTION only; OCAP-off ambient host
  execution is an unmistakable explicit opt-in; authority may attenuate but never widen because an
  environment variable later appears; child processes never inherit the authority switches.
- **Practical caveat (now):** the sharpest vector is closed. `newt solve` previously defaulted to the
  OCAP-**off** full-access Yolo lane purely because `--non-interactive` defaults to true
  (`resolve_lane(false, None, /*non_interactive*/ true) == Yolo`, which set `NEWT_FULL_ACCESS=1` +
  `NEWT_DISABLE_OCAP=1`). **step-3.1** decoupled them: the lane no longer consults `--non-interactive`
  at all; OCAP-off requires the explicit `--unsafe-host-exec` flag (or the `NEWT_UNSAFE_HOST_EXEC` env
  twin), `--confined` still wins, and the **default lane is now `Confined`** (OCAP on, workspace-
  fenced). A plain `newt solve --non-interactive` is confined.
- **Residual:** 🟢 closed. The two halves are now both done: (i) the `--non-interactive` decouple
  (step-3.1) means interaction never selects the OCAP-off lane; (ii) authority is a **typed, immutable
  value** resolved ONCE near startup — `newt_core::launch_authority::LaunchAuthority`. Its
  `from_env()` is the *sole* reader of the three env twins; the entrypoints call it after translating
  their flags, then `freeze()` the result. Deep libraries (`ocap_disabled` / `full_access_requested`
  in `newt-core/agentic/tools.rs`, and the newt-tui policy/banner sites through them) decide via
  `launch_authority::current()` — the FROZEN value — never a live `env::var`. So a `NEWT_DISABLE_OCAP`
  / `NEWT_FULL_ACCESS` / `NEWT_UNSAFE_HOST_EXEC` that appears AFTER startup cannot widen the running
  process's authority. Child-process authority stripping is already covered by `p4-constrained-executor`
  (`CHILD_STRIPPED_AUTHORITY_ENV`).
- **Disabled while open:** n/a (closed).
- **Compensating controls:** `--non-interactive`/`--unsafe-host-exec` decouple (step-3.1); the
  child-env strip (`p4-constrained-executor`); `meet`-only attenuation on `LaunchAuthority` (a later
  context can lower authority, never raise it).
- **Closure criterion:** met — (a) `--non-interactive` cannot select the OCAP-off lane, only the
  explicit `--unsafe-host-exec`; (b) a frozen `LaunchAuthority`, resolved once, is the authority
  source, and a source-inventory gate proves no deep library reads the env twins directly.
- **Ratchet guard:** `non_interactive_never_relaxes_authority` (`newt-cli/src/solve.rs`) — a plain
  headless run resolves `Confined`, never `Yolo`; `frozen_authority_ignores_later_env_mutation`,
  `freeze_is_first_wins_a_second_freeze_cannot_widen`, `meet_can_only_attenuate_never_widen`,
  `from_env_reads_exactly_one_fail_closed` (`newt-core/src/launch_authority.rs`) — freeze a confined
  authority, set every switch env var afterward, and `current()` stays confined; plus the
  `ocap_check.py` source gate (`check_launch_authority_reads`) which FAILS the build if any
  `newt-core/src` file other than `launch_authority.rs` reads an authority env twin with `env::var`
  (a stray deep read re-opens the widen-mid-process hole — verified red on a probe).
- **0.8.0 disposition:** already did not block v0.8.0; this now CLOSES the row entirely (the
  typed-authority follow-on is done, not deferred).
- **Status:** CLOSED — `--non-interactive` decouple (step-3.1) + typed immutable `LaunchAuthority`
  (frozen once, deep-read-free, inventory-gated). Fixed on the (unreleased) 0.8.0 line / epic #749 · owner: — · review-by:
  if a new authority switch is added (route it through `LaunchAuthority`, not a fresh `env::var`).

### p4-constrained-executor
- **Invariant (ideal):** every attacker-influenced subprocess (model shell, build checks, tests,
  formatters, lifecycle hooks, crew ops, MCP stdio, provider/plugin processes, and any git/helper
  that can run repository-authored code) is created through ONE `ConstrainedExecutor` that receives
  explicit origin/trust class, executable + argv, cwd, fs / net / env grants, credentials, timeout,
  and process budget — clearing inherited env, denying net by default, fencing fs to the workspace,
  killing the process tree on cancel, and FAILING CLOSED when the required confinement cannot be
  established. No raw `Command`/`sh -c` bypass may remain for repo-controlled execution.
- **Practical caveat (now):** the confined-spawn seam is now **owned by newt** —
  `newt_core::confined_exec::ConstrainedExecutor` (**step-4.2**). It wraps the audited
  `agent_bridle::ConfinedCommand::spawn` (the same primitive `newt-mcp-client` uses for MCP stdio) and
  adds the fail-closed contract: an `ExecOrigin::AgentInfluenced` request is minted under a **`Kernel`
  strength floor**, so `spawn` REFUSES (`confinement_unenforceable`) whenever the fs fence cannot be
  kernel-enforced — on a kernel without Landlock, or any platform without an OS fs backend — instead
  of running the child unconfined. The child starts env-EMPTY (only explicit grants cross), and
  `workspace_confined_caveats` fences fs read/write to the workspace with an **empty `net`
  allow-list** (kernel deny-all). A real-resource Landlock adversarial test
  (`newt-core/tests/confined_exec_landlock.rs`) proves a hostile child under this executor cannot
  write outside the workspace, read outside it, inherit a parent credential env var, or open a network
  connection — by kernel denial where Landlock is present, by refusal where it is not.
  **step-4.1** already landed the automated no-bypass GATE (`scripts/spawn_inventory.py` +
  `docs/security/spawn-inventory.toml`, CI-wired). **step-4.3** migrated the repo-configured
  `build_check_cmd`, and **step-4.4** migrated crew's `normalize` (formatters) + `run_test` (verify)
  via `run_confined_build` and reclassified crew's remaining spawns (the `git` worktree helper + a
  `gh` read) as `git-helper`. The build fence `build_tool_caveats` now uses a **calibrated read set**
  (workspace + toolchain/package caches, never `~/.ssh`/`$HOME` broadly) — closing a read-then-
  disclose path where a hostile build reads a secret and surfaces it in the tool output the model
  sees — with workspace-only writes (+ explicit operator roots via `build_tool_caveats_with_writes`),
  `TMPDIR` in-fence, net deny-all, and fail-closed off the kernel fence. **step-4.5** migrated the roadmap
  `verify` command (`CommandVerifyRunner::run`, `newt-tui/lib.rs`) onto the executor and reclassified
  that file's remaining spawns (git/gh reads, self-re-exec, human bang-escape) as `trusted-infra`.
  **step-4.6** finished the sweep: the last `agent-exec-todo-p4` sites — the `agentic/tools/shell.rs`
  run_command HOST-SHELL lane — are reached ONLY via an explicit `--disable-ocap`/`--full-access`
  operator opt-out (the default confined posture uses the confined brush engine), so they are
  reclassified `operator-yolo-optout` (confining them would defeat the flag) and **hardened for #8**:
  a shared const table `CHILD_STRIPPED_AUTHORITY_ENV` now excises newt's WHOLE control plane from the
  host-shell child — every authority switch (so a nested `newt` cannot silently re-derive Yolo from an
  inherited `NEWT_UNSAFE_HOST_EXEC`) and newt's own secrets (`NEWT_AGENT_KEY` / `NEWT_OPERATOR_KEY` /
  `NEWT_TOKEN_PASSPHRASE`) — while the operator's general ambient env (their explicit grant) is kept.
  **`spawn-inventory` now shows ZERO `agent-exec-todo-p4` sites** — every attacker-influenced
  subprocess is routed through `ConstrainedExecutor` or the confined brush engine; the remainder are
  classified trusted / git-helper / operator-opt-out and machine-enumerated in `spawn-inventory.toml`.
- **Residual:** 🔴 critical → 🟢 closed for the migration + confinement + env-isolation invariant, and
  child-lifetime containment now lands too (**#1598**, step-8.3): `ExecRequest::timeout` bounds a child
  and the executor **SIGKILLs the child's whole process group** (`killpg`, pgid == the
  `new_process_group` leader) at the deadline AND sweeps the group after completion — a hostile child
  can neither hang the harness nor leave a background same-group descendant running
  (`confined_exec_lifetime.rs`, real-resource). The **setsid / double-fork escape is now closed too**
  (step-8.10): a guarded (opt-in `NetGrant::DenyAll`) child and its whole subtree
  are placed in a delegated **cgroup-v2** subtree (`newt-net-guard` joins it before exec; membership is
  inherited and survives `setsid`), and the executor terminates the tree with **`cgroup.kill`** on
  timeout AND completion — so a descendant that escapes the process group is still killed
  (`net_guard_descendant_lifetime.rs`, real-resource: a setsid session that killpg cannot reach never
  fires). Best-effort/fail-open to `killpg` only where cgroup-v2 delegation is unavailable (that host's
  full-tree containment stays a `b1` residual). Windows #1633 adds the analogous cleanup evidence for
  AppContainer launches: `ConstrainedExecutor` attaches the child to a Job Object with
  `KILL_ON_JOB_CLOSE`, calls `TerminateJobObject` on timeout/completion, and falls back to killing the
  immediate launcher if job assignment is unavailable; `appcontainer_timeout_cleanup_is_distinct_from_authority`
  proves a timed-out PowerShell child returns promptly and does not write a late marker. This is
  lifetime cleanup only, not an authority claim; Windows inherited-handle hygiene remains the separate
  ACTIVE residual `windows-inheritable-handle-leak`. Does NOT block v0.8.0.
- **Disabled while open:** (closed for the routing/confinement bound) — the process-tree-cancel
  residual is bounded by `b1`'s OS sandbox as the backstop.
- **Closure criterion:** met for the migration + confinement bound — `spawn-inventory` shows ZERO
  `agent-exec-todo-p4` sites (every attacker-influenced spawn routed through `ConstrainedExecutor` or
  the confined engine), and the real-resource hostile-child adversarial test proves a build/test child
  cannot read/write outside the workspace, reach the network unauthorized, or inherit a parent
  credential. The surviving-descendant-after-cancel clause is the tracked residual above.
- **Ratchet guard:** `scripts/spawn_inventory.py` (self-tested; CI-gated) — a new or moved
  `Command`/`process::Command` site fails the build until it is inventoried + classified, and a NEW
  `agent-exec-todo-p4` classification re-opens the migration debt; plus
  `newt-core/tests/confined_exec_landlock.rs` (real-resource), the Windows AppContainer evidence test
  `appcontainer_timeout_cleanup_is_distinct_from_authority`, the `confined_exec` unit tests, and
  `host_shell_command_strips_authority_env` (asserts the whole `CHILD_STRIPPED_AUTHORITY_ENV` set is
  excised), which fail if the executor stops confining/failing-closed or the yolo lane stops stripping.
- **Live verifier (register↔verifier↔gate must agree):** `newt_core::ocap::verify_constrained_executor()`
  is no longer a hardcoded `Absent` stub — it reports `Verified` iff the kernel fs fence the executor
  requires is available at runtime (`confined_exec::kernel_fs_fence_available()`), else fail-closed
  `Absent`. `SecurityReport`'s `EnvIsolation` derives from it, so the posture surface tracks live
  enforcement. `newt-core/tests/constrained_executor_truth.rs` ties the three sources: it fails if the
  spawn-inventory gate carries an unmigrated attacker spawn OR if the verifier disagrees with the gate on
  a fence-available host, and a real-resource test proves a parent-only secret never reaches a confined
  child (grounds the `EnvIsolation = Enforced` claim).
- **Status:** CLOSED (migration + confinement + #8 env-isolation) — inventory gate (step-4.1),
  fail-closed executor + kernel-backed adversarial proof (step-4.2), build_check (step-4.3), crew +
  calibrated read fence (step-4.4), roadmap verify + newt-tui reclassify (step-4.5), yolo reclassify +
  whole-control-plane env-strip (step-4.6). `agent-exec-todo-p4 == 0`. Residual: process-tree
  cancellation (bounded by `b1`, tracked **#1598**, does NOT block v0.8.0) · owner: — · review-by:
  when tree-kill-on-cancel lands or `b1` closes.

### mcp-under-leash
- **Invariant (ideal):** every individual MCP tool call is mediated at CALL time before it reaches
  the wire — admission (`mcp-config-admission`) decides *which* servers may connect; this is the
  per-call counterpart. Authority is a leash, not a blanket: an operation the session did not
  authorize does not dispatch, and "no persona" is NOT "unrestricted".
- **Practical caveat (now):** the LEASH invariant is enforced. `McpTools::call` requires a
  `LeasedMcpCall` witness (`newt-core/src/agentic/mcp.rs`, private field, minted only by
  `leash_mcp_call`), so an un-leashed dispatch does not type-check — structurally, like
  `mcp-config-admission`'s `AdmittedServer`. At the sole dispatch choke
  (`agentic/tools.rs`, `execute_tool_inner`) the grant is computed and the witness minted: the
  persona allow-list path is unchanged (allow-listed dispatches; out-of-list is prompted, a deny
  hard-stops). **Authority is a structural [`McpGrant`]** (`PersonaAllowList` | `HumanApproved`),
  minted ONLY from an operator grant — never from the server-chosen tool name. The no-persona path
  is now fully human-gated: a name-classified effect (`classify_mcp_effect`) is shown only as a
  HINT and grants nothing, so a hostile admitted server that renames a destructive tool with a read
  verb (`get_…`) earns no tolerance — it is prompted (interactive) or **denied fail-closed**
  (headless).
- **Residual:** 🟢 closed. Every call is mediated at *dispatch* time (witness-typed leash) and
  authority is a **structural grant, never the tool name** (below). The two prior residuals:
  1. **secret-forwarding** — **narrowed with proof.** A secret value only ever reaches a **trusted,
     operator-configured** server: an *untrusted* origin is refused admission entirely (`admit`, so it
     never spawns/dials/exposes), and even a secret **reference** (`{ env | file | cmd }`) is a hard
     error under untrusted trust (`resolve_secret_under_trust`), so an untrusted server obtains **no**
     newt secret. A trusted server receives exactly the secrets the operator explicitly configured for
     it; the remaining case — a trusted server COMPROMISED post-admission — has its exfil bounded by
     `b1`'s egress floor. The stronger **credential broker/handle** (present the secret to authorized
     outbound requests without the server process ever holding the raw value) is the `b1` egress-proxy
     hardening — tracked under `b1-os-isolation`, not a `mcp-under-leash` blocker.
  2. ~~name-based effect classification is server-influenceable~~ — **CLOSED (structural grant).**
     Authority is an [`McpGrant`] provenance (`PersonaAllowList` / `HumanApproved`) minted only from
     an operator grant; the tool NAME never grants (`classify_mcp_effect` is a display hint), so a
     server renaming a destructive tool `get_…` earns nothing — proven by
     `no_persona_read_verb_tool_is_not_name_granted`. Per-call **budget** (a DoS-bound on a compromised
     admitted server) and **resource-scope** (not newt-enforceable for server-defined args → bounded
     by admission + `b1`) are optional hardening follow-ons, NOT open holes.
- **Disabled while open:** n/a (closed).
- **Closure criterion:** met — an un-leashed `McpTools::call` does not compile; authority is a
  structural grant (never the server-chosen name); and a secret only reaches a trusted,
  operator-configured server (an untrusted one is refused admission AND refused any secret reference),
  with the compromised-trusted-server exfil bounded by `b1`.
- **Ratchet guard:** `no_persona_does_not_dispatch_a_mutating_mcp_tool_unleashed`,
  `no_persona_read_verb_tool_is_not_name_granted` (the name-classification adversarial test — a
  `get_…`-named tool is denied on the name alone, and dispatches ONLY on an explicit human grant),
  `no_persona_mutating_mcp_tool_dispatches_when_human_grants`,
  `remote_tool_outside_allow_list_is_prompted_not_hard_vetoed` (`newt-core/src/agentic/tools_tests/execute_mcp_authority.rs`), and
  `classify_reads_by_verb_prefix_stripping_namespace` +
  `leash_mints_only_from_a_structural_grant_never_the_name` (`agentic/mcp.rs`). Removing the witness
  requirement makes the un-leashed dispatch compile again; re-adding a name-based auto-grant
  re-fails the adversarial test. The secret-forwarding narrow is guarded by
  `admit_denies_untrusted_and_disabled_admits_trusted` + `untrusted_structured_ref_is_rejected`
  (`newt-core/src/mcp.rs`) and `untrusted_env_structured_cmd_ref_is_rejected` (`newt-mcp-client`) —
  an untrusted origin is neither admitted nor able to resolve a newt secret reference.
- **0.8.0 disposition:** does not block v0.8.0 — CLOSED. Closed on the unreleased 0.8.0 line: the
  witness-typed call-time leash + the structural `McpGrant` (authority is never the server-chosen
  name) + the trusted-only admission & trust-gated secret resolution. The stronger credential broker
  (present a secret without the server holding it) and the optional per-call budget are hardening
  follow-ons tracked under `b1-os-isolation` / this entry, not open holes.
- **Status:** CLOSED — witness-typed call-time leash (step-6.4) + structural `McpGrant` authority
  (name-classification vector closed) + secret-forwarding narrowed with proof (trusted-only admission
  + trust-gated secret resolution; compromised-trusted-server exfil bounded by `b1`). Credential
  broker + per-call budget/scope = hardening follow-ons (broker → `b1-os-isolation`) · owner: — ·
  review-by: when the `b1` egress proxy / credential broker lands (may promote this from
  narrow-with-proof to fully brokered).

### posture-report-honesty
- **Invariant (ideal):** every place newt reports its security posture — to the user, to logs, or
  to the model — is DERIVED from the same runtime `verify_*` invariants the fail-closed capability
  gates consult, so a reporting surface can never claim more (or less) than what is actually
  enforced. No hand-written per-lane prose asserts a guarantee the verifiers do not back.
- **Practical caveat (now):** the typed **achieved-security report** exists and is derived, not
  asserted. `newt_core::ocap::SecurityReport` builds one `Achieved` entry per `Guarantee` from
  `RuntimeEvidence::current()` — the very `verify_b1` / `verify_disclosure_gate` /
  `verify_fs_object_bound` / `verify_constrained_executor` / `verify_fail_closed_execution`
  invariants the gates use — with `meet` for compound guarantees (credential/process isolation need
  BOTH the executor and `b1`). There is deliberately **no constructor that takes a free-form claim**.
  `newt doctor` renders it via `security_posture_lines(&SecurityReport::current())` (an
  "Achieved OCAP posture (per guarantee)" block), generalizing the `#1256` "report the achieved
  `SandboxKind`, never the intent" precedent from `newt mcp probe` / the `/mcp` table.
- **Residual:** 🟢 closed for the report type + the `doctor` surface. Follow-up (tracked, not
  claimed here): feed the same report into the remaining hand-written surfaces (the per-turn
  `runtime_context_block` "# Filesystem confinement" claim, the session-start banners) so they too
  read from the report instead of restating it.
- **Disabled while open:** (closed) — a posture surface asserting a claim the verifiers don't back.
- **Compensating controls:** the `verify_*` invariants remain the single source of truth for the
  fail-closed gates; the report is a pure function of them (`SecurityReport::from_parts`).
- **Closure criterion:** met — the report derives every entry from the verifiers, and the `doctor`
  render is a pure function of the report (no independent claim); a guarantee reported `enforced`
  implies its verifier is `Verified`.
- **Ratchet guard:** `linux_report_matches_live_verifier_state`, `compound_guarantees_take_the_meet`,
  `summary_lines_cover_every_guarantee_honestly` (`newt-core/src/ocap.rs`) +
  `posture_lines_are_derived_from_the_report_not_prose` (`newt-cli/src/doctor.rs`). Adding a
  free-form-claim constructor or an over-claiming render breaks these.
- **Status:** CLOSED — step-8.1 (typed `SecurityReport` derived from the verifiers, rendered by
  `newt doctor`) · owner: — · review-by: when the per-turn/banner surfaces are migrated onto it.

### platform-capability-ceiling
- **Invariant (ideal):** newt never claims a security guarantee a platform cannot provide. On a
  platform whose kernel primitives are absent or unverified (no `openat2(RESOLVE_BENEATH)`, no
  Landlock, no proven Seatbelt/AppContainer floor), each affected guarantee is reported
  `unsupported` and any operation that REQUIRES it is refused (fail-closed) — never silently
  downgraded to a best-effort path that still reports "confined".
- **Practical caveat (now):** the report takes the **meet of a pure-data platform ceiling and the
  runtime evidence**, and the ceiling never rounds up. `PlatformCeiling` is one const table per
  platform (`LINUX_CEILING` supports every guarantee; `MACOS_CEILING` / `WINDOWS_CEILING` mark the
  kernel-backed guarantees `Unsupported` with an honest reason; `UNKNOWN_CEILING` — the default
  arm — marks EVERYTHING unsupported, the opposite of a `_ => true` fail-open). Even if every
  runtime verifier were `Verified`, a ceiling entry of "cannot provide" forces `Unsupported`.
  `require_achieved(&report, guarantee)` is the refusal primitive: it returns `Ok` only on
  `Enforced` and a `FailClosed { deviation: "platform-unsupported", … }` on `Unsupported`.
- **Residual:** 🟢 closed for the reporting + refusal contract. This does NOT build the non-Linux
  kernel floors (macOS Seatbelt / Windows AppContainer) — those stay honestly `Unsupported`, which
  is the point: "Linux is the normative fully-supported OCAP platform for this milestone" (no macOS
  runner is needed to represent unsupported truthfully and fail closed). The still-open *runtime*
  fail-open of the non-Linux lexical fs fallback in `tools.rs` is tracked under
  `fs-canonical-containment` (a Linux-closed row) — this row governs the *report/refusal* honesty.
- **Disabled while open:** (closed) — a non-Linux build silently claiming a kernel-backed guarantee.
- **Compensating controls:** unrecognized platforms default to the all-`Unsupported` ceiling;
  `Achieved` has no "best effort" variant (enforced-with-evidence, open-with-deviation, or
  unsupported — nothing else).
- **Closure criterion:** met — an unsupported-platform report never marks a kernel-backed guarantee
  `Enforced`, and `require_achieved` refuses it.
- **Ratchet guard:** `ceiling_never_rounds_up`, `unknown_platform_is_fully_unsupported`,
  `require_achieved_refuses_unverified_and_unsupported`, `current_report_reflects_build_platform`
  (`newt-core/src/ocap.rs`) + `unsupported_platform_never_renders_a_linux_equivalent_claim`
  (`newt-cli/src/doctor.rs`). A permissive default arm or an `Enforced`-on-unsupported path breaks
  these.
- **Status:** CLOSED — step-8.1 (platform ceiling `meet` + `require_achieved` refusal) · owner: — ·
  review-by: when a non-Linux kernel floor lands and its ceiling can flip a guarantee off
  `Unsupported`.

### git-confused-deputy
- **Invariant (ideal):** the harness never runs `git` as a subprocess in a way that lets a hostile
  repository (or a hostile model that wrote into `.git/`) turn an ordinary `git` read into arbitrary,
  UNCONFINED code execution. `git` is a confused-deputy engine — `core.fsmonitor` (fires on
  `git status`), hooks, `diff.external` / per-driver `textconv` (fire on `git diff`), `core.pager`,
  `core.sshCommand`, `protocol.ext` all point ordinary commands at an attacker-named program.
- **Practical caveat (now):** closed. The final OCAP adversarial pass EMPIRICALLY confirmed the escape
  (a repo-local `core.fsmonitor=<payload>` ran out-of-fence on `git status`, inheriting newt's full
  env incl. `NEWT_AGENT_KEY`). **step-7.4** routes every harness `git` subprocess through
  `newt_core::git_hardening::hardened_git`, which `-c`-overrides the gadget keys (beats repo-local
  `.git/config`), `env_clear`s (dropping ambient `GIT_*` gadget vars AND newt's secrets), and points
  `GIT_CONFIG_GLOBAL`/`SYSTEM` at `/dev/null`; diff callers add `--no-ext-diff --no-textconv`. Sites
  migrated: `claim_check` (live cap-exit evidence), `workspace_state` + `lib.rs` context/meta (TUI),
  `acp-worker` diff, `crew`. The model's own `git` tool is the pure-Rust `newt-git` (git2/gix), which
  does not spawn these subprocess gadgets.
- **Residual:** 🟢 closed. Belt-and-suspenders follow-on (NOT required, the gadgets are already
  disarmed): a `.git/`-write guard on the model's fs_write tools so a model cannot even plant
  `.git/config` — tracked **#1602**, not a break because `hardened_git` ignores a planted gadget
  anyway. Does NOT block v0.8.0.
- **Disabled while open:** (closed).
- **Compensating controls:** `spawn_inventory` (a new raw `Command::new("git")` fails the gate until
  reviewed); the `newt-git` model tool is library-based (no gadget subprocess).
- **Closure criterion:** met — a repo-local `core.fsmonitor` gadget does not fire under `hardened_git`
  (real-resource proof), and every workspace `git` subprocess routes through it.
- **Ratchet guard:** `newt-core/tests/git_hardening_confused_deputy.rs` (real-resource, `#[serial]`):
  plants the `core.fsmonitor` gadget and asserts `hardened_git` never runs it while still reading the
  repo correctly. Reverting a site to a raw `Command::new("git")` re-opens the escape.
- **Status:** CLOSED — step-7.4 · owner: — · review-by: if a new harness `git` subprocess is added.

### exec-behavior-bound
- **Invariant (revised, epic #749):** **executable resolution must never widen the capability
  envelope**, and where authority depends on executable identity, that identity must be
  **object-bound** (a resolved executable, not a re-resolvable name). A granted interpreter's exec
  must not become a lever to run a *different*, un-granted behavior tier.
- **Practical caveat (now):** authority in newt is per-REQUEST — `Caveats` (fs/net/exec) are granted
  up front, never derived from the resolved binary — so resolution cannot widen the envelope: a
  confined child runs only what `exec: Scope::Only(...)` granted, and its fs/net behavior is
  Landlock-fenced AND (on every live attacker path) seccomp-egress-denied regardless of *which*
  granted interpreter it is. The one residual is name- vs object-granularity of the exec fence
  itself: Landlock reports `exec` as *interceptor*, not *kernel* (the loader-trampoline corpus is
  shrunk — bin dirs kept out of the read set — but not zero, since `/usr/lib` still hides
  interpreters), so a granted interpreter could in principle `ld.so`-trampoline another program's
  bytes. That widens **nothing** an attacker does not already hold: fs and net are object-bound and
  kernel-fenced, so a trampolined tier gets the same (denied) fs/net envelope.
- **Residual:** 🟡 — name-granularity exec trampoline, bounded by the object-bound fs/net fences and
  `b1`'s OS floor. No reachable authority-widening path.
- **Disabled while open:** nothing distinct — no capability becomes reachable only by closing this;
  it is a hardening refinement of an exec fence that is already object-bound on fs/net.
- **Compensating controls:** per-request `Caveats` (authority never depends on binary identity);
  Landlock `exec: Only` (only granted programs run); object-bound, kernel-enforced fs/net fences;
  `spawn_inventory` gates any new raw spawn.
- **Closure criterion:** kernel-granularity exec (W^X + a micro-VM rootfs, or seccomp `execve`
  argument binding) that makes the trampoline unrepresentable — the same OS floor `b1` pursues.
- **Ratchet guard:** bounded by `b1`; no dangerous capability is gated *solely* by this entry, so no
  OCAP-DANGER site names it (the exec fence is structural — Landlock `exec: Only` — not a runtime
  `require`). Reverting an `AgentInfluenced` spawn off the `ConstrainedExecutor` fails the
  `spawn_inventory` gate.
- **Bounded-by:** `p4-constrained-executor`, `fs-canonical-containment` — the CLOSED invariants that
  make the residual harmless: every `AgentInfluenced` exec is routed through the kernel-fenced
  `ConstrainedExecutor` (object-bound fs/net), and fs authority is canonical-path object-bound. A
  trampolined tier inherits that same (denied) fs/net envelope. `ocap-check` (`check_state_proofs`)
  requires both bounds to be CLOSED, so if either reopens, this BOUNDED claim fails.
- **Status:** BOUNDED — unlike a GATED entry this capability (executing a granted interpreter) IS
  reachable; it is not *unreachable*. What holds is that binary resolution cannot WIDEN the already
  object-bound authority envelope: the name-granularity exec-trampoline residual is bounded by the
  CLOSED fs/net invariants named above (`Bounded-by`), so a trampolined tier gets no authority the
  child did not already hold. CLOSES to a kernel-granularity exec fence with `b1`'s OS floor. owner: —
  · review-by: with `b1` / epic #749.

## 5. How to use this (for the practical-caveat moments)

When you must cut a corner to get function:
1. **Name it here** as a deviation (don't let it be silent).
2. State **what it disables** (the dangerous capability that goes fail-closed) — that *is*
   the bound; the function you keep is bounded-safe.
3. Wire the **ratchet guard** so the bound is enforced by the system, not by memory.
4. Write the **closure criterion** as a runtime check.
5. `ocap-check` then holds the line; closing the deviation later is a single ratchet click
   that unlocks the capability — convergence back to the proper OCAP vision, by construction.
