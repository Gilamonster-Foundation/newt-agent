# Changelog

Notable changes to the newt-agent workspace. Format follows
[Keep a Changelog](https://keepachangelog.com); versioning is Semver
(`0.MINOR.PATCH`, pre-1.0). The workspace version in the top-level `Cargo.toml`
is inherited by all internal crates.

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
