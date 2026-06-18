# Config scaling, deployment portability, and the project-config trust gate

**Status:** Design (2026-06-18). The config foundation under the crew front door
(`crew-front-door-and-workflow-tui.md`). Implemented in phases (see that doc's
build order). Decisions locked in planning: committed project config + trust
gate; cycle-safe resolver now, cross-links later; infer the test command.

## 1. Why now

Crews are about to multiply, and a crew references role-loadouts which reference
backends/bundles/profiles/personas. That graph will get tangled, and the configs
must live somewhere sane across very different deployments. We settle three
things before the crew configs land: **how configs are discovered and scale**,
**how they stay portable from a human laptop to a headless k8s container**, and
**how a committed project config stays safe** (it's read with ambient authority
at bootstrap).

## 2. Two amphibious deployment targets (design for both)

The human-facing TUI we build first is the **test/design** environment. A
first-class target is **headless deploy in a k8s container**. The config system
must serve both, which makes these first-class, not afterthoughts:

- **Everything env / path overridable.** A custom container image needs to
  relocate env, `PATH`, the **WORKDIR**, and the config roots. The triad maps
  onto container mounts:

  | Triad layer | Laptop | Container |
  |-------------|--------|-----------|
  | `/etc/newt/` | system-wide | a **ConfigMap** mount (read-only org defaults) |
  | `~/.newt/` | user config + state | a **PVC** (persistent user/agent state) |
  | `<cwd>/.newt/` | project config | the project **workdir PVC** |

  `$NEWT_CONFIG` already overrides the base config path. We add a **`NEWT_HOME`**
  override so `~/.newt` can be relocated when `$HOME` is read-only/ephemeral in a
  container (the loader uses one `newt_home()` helper instead of `home_dir()`
  directly).
- **PV / PVC for writable state.** Crew worktrees (`./.newt/worktrees/`), caches
  (`~/.newt/{cache,tmp}/`), and history land on persistent volumes — the
  container's ephemeral fs won't do. Keeping all writable state under two
  relocatable roots (`NEWT_HOME` and the workdir) is what makes that mountable.
- **Mixed provider kinds in one pool.** Already supported: `BackendKind::Ollama`
  / `Openai` backends + subprocess `ProviderConfig` (Anthropic, etc.).

### The canonical crew loadout (the motivating example)

**Claude + OpenAI + Local Inference together, routed by cost/capability tier:**
reserve **frontier models (Claude/OpenAI) for planning + diagnostics** (the
high-value, expensive-per-token roles — planner, triage-of-hard-failures), and
push **local inference for the slow-but-cheap "plug and chug"** bulk work
(long-running, budget-friendly, lower-intelligence edits). This is precisely what
the heterogeneous, availability-adaptive `BackendPool` + crew role-routing is
for; the crew config (`[crews.*]`) is where a human declares that tiering.

## 3. Discovery & scaling — crews as files across the triad

Extend the existing per-file discovery (`merge_disk_{bundles,loadouts}`) to
**crews**: `crews/*.toml` under each triad layer, filename stem = name,
cwd-most-specific wins, last-wins on a clash. `[crews.<name>]` inline tables
still work. No new mechanism — same `Config::resolve()` path. (We also extend
bundles/loadouts/crews discovery to include `/etc/newt/` for the ConfigMap case;
today they only scan `~/.newt` + project.)

## 4. Cycle-safe reference resolver

Today the reference graph is shallow and acyclic by construction, and validation
(`Loadout::validate`) is non-recursive. Adding crews — and, later, `extends` /
`include` cross-links — makes cycles possible. So we build a **generic
cycle-safe resolver now**:

- Walk the typed reference graph
  (`loadout → {backend, bundle, profile, role}`, `bundle → profile`,
  `crew → role-loadouts`) with a `visited: HashSet<(Kind, Name)>`.
- On revisiting a node on the current path, error with the **cycle path**
  (`crew:coder → loadout:planner → … → crew:coder`).
- Track `name → source-file` (TOML 1.0 has no spans) so dangling/cyclic errors
  name the offending file.
- References stay **flat** for now; the resolver is the seam that makes
  `extends`/`include` safe to add later (a tracked follow-up).

## 5. Project-config trust gate (committed, but safe)

Project config (`<cwd>/.newt/*`) is **committed** (so crews/loadouts travel with
the repo and agents share them) — but it is read **unconfined at bootstrap**,
before caveats are minted (bootstrap configures authority, so it can't be gated
by it). A committed `./.newt/config.toml` in a *cloned* repo could therefore
point `soul_file` / `api_key_file` / backends / (future) includes at arbitrary
paths with ambient authority. So:

- When project `./.newt/*` config exists and the project dir is **not approved**,
  newt **does not load it** (falls back to `NEWT_HOME`/`~/.newt` + `/etc/newt`)
  and prints: `untrusted project config at ./.newt — run 'newt trust' to load it`.
- `newt trust` records the project path + a **content hash** in
  `$NEWT_HOME/.newt/trusted.toml`. A hash change (config edited) **re-prompts** —
  approval is for *this* content, direnv-style.
- Headless/container note: a deploy that mounts a known-good project config sets
  `NEWT_TRUST=1` (or pre-seeds `trusted.toml`) to skip the interactive gate —
  the container image is the trust boundary there, not a human.

### Gitignore convention

Commit `./.newt/*.toml`. Gitignore `./.newt/{cache,worktrees,local}/`. Per-dev
overrides via `./.newt/local/*.toml` (gitignored, merged last). Deletable caches
in `$NEWT_HOME/.newt/{cache,tmp}/`. `newt init` writes a `./.newt/.gitignore`
with these rules.

## 6. Out of scope (tracked)

- `extends` / `include` cross-links — the resolver is already cycle-safe; enable
  on need.
- `--extra-dirs` to widen `fs_write` beyond cwd (doesn't exist yet; crew
  worktrees stay under cwd so it isn't needed for crews).
- Helm chart / container image / volume manifests — a deploy concern, separate
  from this config-code work; this doc just keeps the code portable for it.

## 7. Verification

Unit tests: crew directory-discovery + triad precedence; the cycle-safe resolver
(synthetic cycle → error names the path; dangling ref → names the file;
`NEWT_HOME` relocation honored); the trust gate (untrusted project config
refused; `newt trust` records + loads; changed hash re-prompts; `NEWT_TRUST=1`
bypass). All with `tempfile`, no network.
