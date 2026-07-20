# Changelog

Notable changes to the newt-agent workspace. Format follows
[Keep a Changelog](https://keepachangelog.com); versioning is Semver
(`0.MINOR.PATCH`, pre-1.0). The workspace version in the top-level `Cargo.toml`
is inherited by all internal crates.

## [Unreleased]

### Security

- **Trust boundary on MCP secret resolution (#1301 review).** `${…}`
  interpolation and `{ env | file | cmd }` references now resolve host-side
  **only for newt-owned (TRUSTED) config** — `config.toml`'s `[[mcp_servers]]`
  and `~/.newt/mcp.toml`. A **discovered Claude/project overlay**
  (`~/.claude.json`, `<workspace>/.mcp.json`) is **UNTRUSTED**: its literal
  env/header values pass to the child **verbatim** (no interpolation, no
  `cmd:`/`file:` execution — the pre-#1301 behavior, which also restores
  Claude-overlay compatibility), and a structured `{ env | file | cmd }`
  reference is **rejected**. This closes a host-RCE / confinement bypass: a
  cloned repo's `.mcp.json` with `env = { Y = "${cmd:…}" }` (or a `{ cmd = … }`
  ref) can no longer run a command on the host, unconfined, at MCP connect.
  Provenance is stamped at discovery (`McpServerEntry::trust`); `newt mcp
  import` writes into `~/.newt/mcp.toml`, so an imported server is adopted as
  trusted by explicit operator gesture.
- **Conservative interpolation contract.** For trusted values, a `${…}`
  interpolates only when it is a known scheme (`${env:…}`/`${file:…}`/`${cmd:…}`)
  or a bare `${IDENT}`; any other `${…}` (`${VAR:-default}`, `${.field}`) passes
  through **verbatim** instead of hard-failing (fixes an upgrade regression on
  pre-existing configs). `$${` escapes a literal `${`. A recognized reference
  that resolves to nothing stays a loud error.
- **Redaction hardening.** `newt config`'s `to_redacted_toml` now also redacts
  credentials embedded in an MCP `url` (userinfo `user:pass@`, sensitive query
  params) and in `args` (`--token`/`--api-key`/… values). Interpolation errors
  (e.g. an unterminated `${`) no longer embed the raw value in the message.
- **`newt mcp import` correctness.** Import now checks name clashes against the
  full merged config: a name already defined in `config.toml` (which outranks
  `~/.newt/mcp.toml`) is a loud error instead of a silently-shadowed write. A
  plain `newt mcp import` always targets `~/.newt/mcp.toml`, never an ambient
  `./newt.toml` in the current directory.

### Added

- **`~/.newt/mcp.toml` + Claude-JSON import + secret references on MCP
  `env`/`headers` + scrybe.ai smart-install (#1301).** MCP config is now
  first-class and secret-safe:
  - **`~/.newt/mcp.toml`** — a dedicated newt-owned source (same
    `[[mcp_servers]]` schema as `config.toml`), discovered by
    `newt_core::mcp::discover` as a newt-owned layer ranked with `config.toml`
    `[[mcp_servers]]`, above the borrowed `~/.claude.json` / `<ws>/.mcp.json`
    overlays; missing/malformed is non-fatal. `newt mcp add|install|import`
    prefer it once it exists (else keep writing `config.toml`, #1291's
    behavior, honoring `--config`/`$NEWT_CONFIG`/`./newt.toml`/`--project`);
    `newt mcp list` attributes each row to its source (`newt mcp.toml`).
  - **`newt mcp import <path>` / `--from-claude`** — reads a Claude-Code
    `mcpServers` JSON (via `parse_claude_mcp`) and writes the equivalent
    `[[mcp_servers]]` TOML through the comment-preserving writer, breaking
    config out to `~/.newt/mcp.toml` (created if absent). Dedup-by-name:
    error on a clash by default, `--force` overwrites, `--merge` skips
    existing. Secret-bearing values (incl. Claude's `${VAR}`) import verbatim.
  - **Secret references on every `env` and `headers` value — both syntaxes.**
    A value is now a `SecretValue`: a plain string (`Literal`, backward-compatible
    with every existing config and with Claude JSON) or a `{ env | file | cmd }`
    table (`Ref`, the existing `SecretRef` scheme). In **trusted** (newt-owned)
    config a literal may embed `${...}` interpolation tokens —
    `${VAR}`/`${env:VAR}` (env), `${file:~/p}` (first non-empty line),
    `${cmd:vault kv get -field=token secret/x}` (command stdout — the Vault
    path) — with literal text around tokens preserved (see **Security** above
    for the trust boundary that gates this). A recognized but
    missing/empty reference is a hard error at spawn (never a silent empty).
    Resolution happens **host-side, wrapped in `Secret`**, right before the
    confined stdio child's env is built (`newt-mcp-client`) and the HTTP
    headers are built — never inside the confined child, never into newt's own
    process env. `newt config`'s `to_redacted_toml` redacts literal values
    (incl. `${cmd:…}` strings) and keeps `{ env|file|cmd }` references (they
    name a location, not a secret).
  - **`newt mcp install scrybe`** resolves the `scrybe-mcp-server` binary
    across `PATH` → `~/venv/bin` and registers it by **absolute** path (survives
    PATH changes); a missing bundled-scrybe binary is a clear error naming
    `pip install scrybe.ai` (a hint, never an auto-`pip`).

- **`newt mcp probe` — derive a server's registration by asking the server
  (#1292).** Probing is verify-and-enrich, never discovery-by-scanning: the
  operator names a stdio command or an `http(s)://` URL. A stdio candidate is
  spawned under the **shared confined probe leash** (`Config::mcp_probe_caveats`
  — doctor's policy, never `top()`; exec widened by exactly the probed
  command, #1256), trying the pure-data arg-spelling rules (`stdio`, none,
  `mcp`, `serve`; overridable via `mcp-probe-rules.toml`) when `--arg` is
  omitted. A URL is dialed as streamable-HTTP through the egress proxy with
  the `net` axis widened by exactly the typed host (#1267); non-loopback
  plain-http needs `--allow-http` and still warns, HTTP 401/403 reports "run
  `newt auth <name>`", and a rejected initialize is flagged as a likely
  legacy-SSE-only server. The report carries the server's self-reported
  identity (`newt-mcp-client` now captures the `initialize` result —
  serverInfo + instructions — instead of discarding it), tools, and the
  honest sandbox/net posture; `--json` is machine-readable. `--save` writes
  through the `newt mcp add` path; `--to-catalog` upserts a comment-preserving
  `mcp-catalog.toml` entry that `newt mcp install` can replay. Execution and
  writes confirm `[Y/n]` (`--yes` to skip; non-TTY fails closed).
- **`newt mcp` management verbs — no more hand-editing `[[mcp_servers]]`.**
  `newt mcp add <name> --command … [--arg …] [--transport stdio|sse|http]
  [--url …] [--env K=V] [--timeout-secs N]` and `newt mcp remove <name>` edit
  the same config file `Config::resolve()` reads as its base (`--config` >
  `$NEWT_CONFIG` > `./newt.toml` > user config; `--project` → the nearest
  ancestor `.newt/config.toml`) through pure, comment-preserving `toml_edit`
  writers — duplicate adds, absent removes, and an unreadable existing config
  error loudly (never rewriting a file that cannot be read back). `newt mcp list` renders the merged discovery view (newt config
  + Claude Code overlays) with per-row source attribution, flagging entries
  discovery would drop. `newt mcp install <name>` resolves a curated pure-data
  catalog (bundled `scrybe` entry; overridable via `~/.newt/mcp-catalog.toml`
  and project `.newt/mcp-catalog.toml` drop-ins, merged by name). Bare
  `newt mcp` still serves newt-as-an-MCP-server over stdio, unchanged.

## [0.7.3] — 2026-07-11

**Theme: release-pipeline repair.** v0.7.2's crates.io publish stage broke at
`newt-core` — its `newt-tuner` dependency (new in Step 19.6) was path-only
(no version requirement), which `cargo publish` rejects, and `newt-tuner` was
missing from the publish matrix entirely. Only `newt-skills`, `newt-data`, and
`plugins-protocol` reached crates.io at 0.7.2; the tag can't be reused, so
0.7.3 re-ships 0.7.2's content with the pipeline fixed. No functional changes.

### Fixed

- **`newt-tuner` now publishes** (#1091): inherits the workspace version,
  carries a `=` pin in the workspace dependency block, and sits in the publish
  matrix ahead of `newt-core`.
- **`check-publish-order` blind spot closed** (#1091): the pre-flight treated
  every `req == "*"` dependency as a stripped dev-dep; it now errors on
  path-only NORMAL/BUILD deps (a hard `cargo publish` failure), keeps
  version-pinned dev-deps in the ordering graph, and flags wildcard reqs
  pointing outside the workspace.

### Changed

- **Dependency refresh via the lock**: agent-mesh 0.6.3 (fail-closed
  causal-generation enforcement §9.1, proof-of-possession for external-pubkey
  certification §9.2, atomic 0600 UserKey creation) and agent-bridle 0.7.2 —
  both semver-compatible with the existing `"0.6"` / `"0.7"` pins.
- Stray top-level `newt-tuner/` scaffold removed; the crate README now lives
  in `crates/newt-tuner/` (#1092).

## [0.7.2] — 2026-07-11

**Theme: plans within plans — a durable multi-conversation workspace and the
#1030 roadmap tree, made portable as repo-owned data.** newt gains a real
notion of multiple conversations per workspace and an objective, git-grounded
roadmap it can drive node by node — then `/roadmap export`/`import` lift that
tree out of the per-machine store into a checked-in file.

### Added

- **Multi-conversation workspace (#1030).** Each `newt` process mints and claims
  its own conversation; a `live_owners` table enforces one live owner per
  conversation (unix `kill(pid,0)` / Windows `OpenProcess` liveness, a crashed
  owner's stale claim reclaimed on next attempt). New verbs: `/start [title]`,
  `/rename <title>`, `/end` (finalize & continue in place), and `/resume
  [query|n|id]` — an MRU list annotated `● live` / `○ open` / `✓ ended` with
  FTS5 search. Fresh-on-launch by default (no more auto-glomming onto a folder's
  latest transcript). (#1036–#1039)
- **The "Plans within Plans" roadmap tree (#1030).** A persisted `roadmaps`
  table holds a `Roadmap → Phase → Plan → Task` tree (a serialized `plan.rs`
  `Plan`); `conversations` gains thin nullable `roadmap_id`/`node_id` pointers
  (additive migration; the §6 hash chains still verify). `/roadmap`
  (`new`·`list`·`show`·`use`·`add`·`next`·`bind`·`done`·`eval`·`drive`) and
  `/tree` author and render it, with a DFS cursor and resume-to-cursor. (#1038,
  #1039)
- **Objective node evaluators (#1030).** A pure reducer decides done-ness from
  *objective* state, never self-report: Task = commit-on-branch + verify; Plan =
  children-done + verify; Phase = children-done + PR merged; Roadmap =
  children-done + CI green. Missing remote facts degrade to `Unsupported` — never
  a false `Done`. Local git truth via `newt-git`; forge/CI via `gh`. (#1040,
  #1043) Plus the headless **`TreeDriver`** (`/roadmap drive`): evaluate the
  cursor, ripple completion up the tree, halt at the first node that still needs
  work. (#1044)
- **Roadmap-as-code (#1082).** `/roadmap export [path]` / `import [path]`
  (default `.newt/roadmap.toml`) move the tree between the store and a checked-in
  file — the repo is the authority, the store a working copy; a fresh checkout
  bootstraps with one command.
- **Issue-close forge-fact eval gate (#1083).** `/roadmap issue <node> <#>` binds
  a node to the forge issue it realizes; `/roadmap eval` then additionally
  requires that issue **closed** before the node may be `Done` (a verdict input,
  never a direct `Done`), via `gh issue view`.
- **Personas & MCP.** A personal-assistant persona and a `/persona` switch verb,
  an MCP toolset connector, a headless persona loader + headless MCP persona, a
  `skills` field on `RoleProfile`, and the `gila` / `gila-personal-assistant`
  skills with untrusted-MCP-result wrapping. (#1045–#1058, #1021/#1042)
- **DGX.** An interactive vLLM model switcher (`newt dgx models`) and the DGX
  unboxing plan. (#1032, #1055)

### Fixed

- **Roadmap import could steal a roadmap across workspaces (#1086).** The
  `roadmaps` primary key is now composite `(id, workspace_key)` (with an
  idempotent, lossless rebuild migration), so an import of an id that exists
  under another workspace inserts a separate row instead of replacing it — the
  write path is now workspace-fenced like the read path. (#1087)
- **`pid_is_alive` liveness probe** gated its `libc::kill` call to unix and added
  a Windows `OpenProcess` path. (#1037)

## [0.7.1] — 2026-07-06

**Theme: harness-graded correctness, an honest shell-authority story, and
first-class OS installers.** Opens the 0.7.x line by adopting agent-bridle 0.7.1
wholesale (OCAP shell engine seam + carried coreutils) and shipping the
result-oracle evaluator.

### Added

- **Result-oracle evaluator (`output_matches`, epic #957).** newt-eval can now
  grade *what a program prints*, not just its diff. A case declares
  `expected_output` + an `[output_match]` run command; the oracle executes it and
  compares stdout through a pure-data **normalization pipeline** (`trim`,
  `collapse_whitespace`, `trailing_newline`, `regex_extract`, `numeric_tolerance`
  with `epsilon`). Behind an injected `CommandRunner` seam: mocked in the per-PR
  unit tier, real `SubprocessRunner` (hard timeout, honest "runtime not found")
  in a new weekly/release tier (`output-oracle-real.yml`). Opt-in per case; the
  graded-code trust boundary is documented (see #887). (#958, #959, #960)
- **Selectable `run_command` shell engine** — `safe-subset` / `host` / `brush`
  behind the agent-bridle ADR-0005 D2 seam; `--full-access` auto-selects `host`
  (`brush` on Windows for internal-tooling compatibility). Carried coreutils are
  enabled cross-platform via dispatch-capable binaries. (#868, #951)
- **OS installers.** brew formula, Chocolatey `.nupkg`, RPM, and DEB packages of
  the `newt` + `newt-mcp-server` binaries, built in the release workflow and
  attached to the GitHub release.

### Changed

- **`--full-access` prose reworked** (#926). The help text now states plainly
  that `--full-access` lifts the permission preset but is a *distinct* switch from
  `--yolo` (the shell still honors the active exec floor), and that it does not by
  itself grant Codex/Claude-Code-style ambient authority. `newt doctor` explains
  engine selection.
- **`gather_code_files` honors the language-pack extension allowlist** instead of
  a hardcoded `rs`/`py` filter (#956).

### Deferred

- Async TUI steering + live command output + Ctrl+T transcript (#952) → 0.7.2.

## [Unreleased] — targeting 0.6.9

**Theme: the crew / team / overseer orchestration stack + honest, attenuating
authority.** A conversational overseer plans, composes a roster, dispatches crews
per step, and reviews — under capability leashes that only ever *attenuate*.

### Added — multi-LLM orchestration (`newt-scheduler`, ROADMAP Phase 23)

- **crew / panel / team / roster** — `run_crew` (role-routing loop, #425), `run_panel`
  (N decorrelated voices, verify-gate each, anti-groupthink, #468), `run_team` (a lead
  decomposes a goal → a crew per subtask, per-subtask verify, #474/#477), and the
  `compose_roster` composer (survey live models → propose role→model with a rationale,
  #480).
- **hosted-LLM dispatch** — `BackendKind::Openai` over `/v1/chat/completions` + bearer
  `api_key`, so crews/teams run on a hosted model, not just local Ollama (#478).
- **agent-callable crew/team tools** — the async `CrewRunner` trait + `compose_roster`
  / `crew` tools behind the `NEWT_TEAM` toggle; `LocalCrewRunner` injected into the
  scheduler-free TUI loop (the inversion: newt-cli owns the scheduler) (#479/#482/#484).
- **verified work lands as a git branch** — a passing crew commits to a `crew/<id>`
  branch in the shared object store (review/merge with the embedded `git` tool);
  unverified work is isolated and discarded (#489).
- **per-member fs_write leash** — `run_crew`/`run_team` enforce `Caveats` at the apply
  step: a member's out-of-leash edits are refused (attenuation, never amplify) (#494).
- **crew step-up authority (23.2)** — `crew_attest` decision surface (`crew_step_up_policy`
  + `crew_authz → {Allow, NeedsAttest}`, #508) wired onto the live dispatch path:
  `LocalCrewRunner` consults it before any effect, so a crew/team dispatch (an *amplify*,
  §7.5) is held for a human attestation (#517). Structure now — the `/team` enable maps to
  `Presence::Prompt`, so it allows today; real passkey teeth arrive with BOOT (#472).
- **MCP git dev-kit** — a `git` tool on `newt-mcp-server`, so any MCP client can drive
  a full coder out-of-band (#469).
- **tiered verify gate** — `VerifyTier` + one honest cap-exit banner (#470).
- **architecture doc** — `docs/design/crew-swarm-overseer.md`; ROADMAP Phase 23.

### Changed — dependencies

- **agent-bridle `[patch.crates-io]` advanced `feat/stub-shell` →
  `feat/step-up-decision-mvp`** (newt#497): the agent-bridle branch that **actually
  carries** the human-presence `step_up` Gate (agent-bridle#24) ROADMAP 23.2 builds
  on, and is brush-free (crates.io-safe). `feat/stub-shell` *and*
  `brush-stub/publish-unblock` both lack `step_up`; `main` has it but carries the real
  brush shell (dev-only, forbidden by the CI stub guard, so the pin can't sit there).
  The `just shell-stub`/`shell-real` toggle now targets this branch. **Interim
  (Path B)** — it's a feature branch; revisit before release; the canonical fix is
  folding `step_up` into the durable stub branch `feat/stub-shell`. Full workspace
  green, no API drift.

### Fixed

- **backendless config** no longer re-grows a synthesized `ollama` backend when only
  per-file drop-ins are present; amber harness notices (#492, recovering a regression
  the #473 squash dropped).
- **flaky pre-push gate** (#516, closing the primary cause of #507) — the cw-400 recovery
  test swapped the process-global `$HOME` to isolate the probe cache, racing ~20
  HOME-reading tests (intermittent EACCES under the workspace-wide instrumented run). The
  cache now redirects via a `#[cfg(test)]` thread-local, touching no global state. Residual
  `set_var`/`getenv` test-isolation work is tracked in #514 (the CLAUDE.md testing tiers).

### Release action when cutting 0.6.9

- Bump `[workspace.package] version` 0.6.8 → 0.6.9.
- The `[patch.crates-io]` agent-bridle block is still **git-pinned** (`branch =
  "feat/step-up-decision-mvp"` — a **feature branch**, so the most interim of the
  pins). When agent-bridle 0.1.0 is indexed on crates.io, drop the block and
  `cargo update -p agent-bridle` (agent-bridle#20). **Decide before release (Path B is
  interim):** fold `step_up` into the durable stub branch `feat/stub-shell` (the
  canonical fix) and revert the convention. Do **not** revert the pin to
  `feat/stub-shell` as-is (it lacks `step_up`).

## [0.6.8] — 2026-06-14

**Theme: an honest, measurable, family-aware harness.** This release turns the
harness from something we *tuned by hand* into something we can *measure* — and
the first measurement (below) is the milestone: the "fabricate an entire API
surface under context overflow" failure is **model-family-specific, not
structural**. That reframes the project's goal as a harness that adapts its
support per model family.

### Milestone result

- **Cross-family confabulation finding** (`docs/findings/2026-06-14-cross-family-confabulation.md`).
  Driving the identical "one Python example per PyO3 crate" task through the new
  ground-truth rig: **both nemotron models fabricate the entire import surface
  (score 0.0)** while **`qwen3-coder:30b` resolves it correctly (1.0)** — same
  harness, same corpus, same prompt. Evidence that newt's *support harness*
  should be tuned per family. Seeds the paper *"Newt-agent: an agent for Nemotron."*

### Added — the verify / measurement program (#332, #75)

- **Verify oracle** (`newt-core::symbols`): a language-agnostic symbol index with
  `resolve`/`classify` and a two-stage *not-built* vs *fabricated* discriminator;
  **Python and Rust adapters** (general-first). Catches the fabricated-reference
  class a blind `py_compile` cannot see (#339, #341).
- **`python_imports` evaluator** + **`newt-eval score`**: score any workspace's
  Python output against a declared module surface, standalone of the case fixture
  framework (#340, #342).
- **Ground-truth stress rig** (`docs/testing/results/scripts/`): `pack_pyo3_corpus.sh`
  (assemble N PyO3 crates into a working-set corpus — the overflow knob) +
  `rig_pyo3_examples.sh` (snapshot → drive newt headless → score → scorecard, with
  a DGX-free dry-run) (#343, #344).
- **Canonical `Plan`/`Subtask` serde struct** (`newt-core::plan`): one shared
  source of truth for `/plan` and the swarm scheduler — fragment-valid,
  default-deny `caveat_policy`, resumable status/result (#338).
- **Agent commit identity** (`.newt/agent-identity.toml` + `newt identity`):
  configurable per-agent committer (`newt-agent[bot]` default), secrets-by-reference,
  never inline (#329).
- **Progressive-disclosure memory** (`memory_fetch` + budgeted index) — the #319
  follow-through (#328).
- **#319 re-read breadcrumb**: summarized file reads are no longer silently
  hallucinated — compression names the dropped files and instructs a re-read (#321).

### Added — memory, recall & continuity (Phases 17–19)

- SQLite conversation store with §6 causal ordering; one-time JSON import (#261, #265).
- FTS5 recall index + `recall`/`save_note` tools + memory nudge (#268, #271, #262).
- Compression v2 — *summarize, don't discard* — structural prune, continuity
  restore, `/compress` + anti-thrash visibility (#267, #251, #275, #280).
- Auto-resume, `--ephemeral`, `NEWT_CONVERSATION_ID`; tool-event + token-usage
  recording (#273, #272).
- Write-time security scan for agent notes; NoteStore v2 (#259, #250).

### Added — self-tuning (Phase 20) & data co-pilot (Phase 21)

- Model self-tuning from observed usage + active `/probe` capability discovery —
  the seed of a per-family auto-tuner (#313, #320).
- `newt-data` engine (SQLite EDA), `newt-mcp-data` server, PyO3 `newt_data`
  submodule, live Jupyter-kernel co-pilot, notebook persistence, dataframe
  introspection (#326, #327, #330, #331, #333, #336).

### Added — platform

- Agentic loop extracted to `newt-core::agentic` (reusable beyond the TUI) (#253).
- `AGENTS.md`/`CLAUDE.md` loaded into the system prompt (#242).
- Streamable-HTTP MCP transport (#241); OpenAI provider plugin (#252).
- Personas / `RoleProfile`; named permission presets + `/mode` (authority floor);
  reusable cowork turn-driver (#227, #312, #310).
- Project-local `.newt/config.toml` overrides; per-session plan files (#236, #239).
- ADR: plain-scroller TUI, amphibious by design (#304).

### Fixed

- Context-window: first-turn requests respect the `num_ctx` ceiling; graceful
  400 recovery; trailing-group protection survives interleaved messages (#284,
  #235, #295).
- Dev/release: `install-hooks` rewrites pushes to HTTPS + drains stdin;
  topological publish order + pre-flight check; lockfile dep sync (#286, #287, #296).
- Test deflaking: mDNS announce window, mock-plugin ETXTBSY race (#293, #290).

### Notes for packagers

- All internal `newt-*` crates inherit `version.workspace = true`; the `newt-mesh`
  sub-workspace is versioned in lockstep.
- crates.io publish requires the agent-bridle **stub-shell** toggle (`just
  shell-stub`); `main`/release must not carry the brush git dep.
