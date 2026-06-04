# Code Audit Findings — Top 4 Issues

Filed from a post-refactor smoke test + code review session (2025-07).
Each section below is a self-contained issue ready to paste into `gh issue create`.

---

## Issue 1 — `DEFAULT_SOUL` in `newt-core` is stale: missing `use_skill` and `web_fetch`

**File:** `newt-core/src/memory.rs`, line ~587

```rust
const DEFAULT_SOUL: &str = "\
You are newt, a small, fast, local-first agentic coder. \
Be concise and direct. \
You have tools: run_command, read_file, write_file, list_dir. \
Use them to actually complete tasks rather than describing what to do.";
```

### Problem

`use_skill` (#135) and `web_fetch` (#139) were added after this constant was
written. Sessions that load from `DEFAULT_SOUL` (i.e. no `soul.md` file in the
workspace or `~/.newt/`) silently give the model an incomplete self-description
that omits two real tools. The model may not call `use_skill` or `web_fetch`
because it doesn't know they exist.

### Fix

Update `DEFAULT_SOUL` to list all six tools:

```rust
const DEFAULT_SOUL: &str = "\
You are newt, a small, fast, local-first agentic coder. \
Be concise and direct. \
You have tools: run_command, read_file, write_file, list_dir, use_skill, web_fetch. \
Use them to actually complete tasks rather than describing what to do.";
```

### gh command

```bash
gh issue create \
  --title "fix(soul): DEFAULT_SOUL in newt-core is stale — missing use_skill and web_fetch" \
  --body "## Problem

\`newt-core/src/memory.rs\` contains a hard-coded \`DEFAULT_SOUL\` constant used when no \`soul.md\` file is present:

\`\`\`rust
// line ~587
const DEFAULT_SOUL: &str = \"\\
You are newt, a small, fast, local-first agentic coder. \\
Be concise and direct. \\
You have tools: run_command, read_file, write_file, list_dir. \\
Use them to actually complete tasks rather than describing what to do.\";
\`\`\`

\`use_skill\` (#135) and \`web_fetch\` (#139) were added after this was written. Any session that falls through to the built-in default (no workspace \`.newt/soul.md\`, no global \`~/.newt/soul.md\`) gets an incomplete tool list. The model may not call \`use_skill\` or \`web_fetch\` because it doesn't know they exist.

## Fix

Update \`DEFAULT_SOUL\` to list all six tools:

\`\`\`rust
const DEFAULT_SOUL: &str = \"\\
You are newt, a small, fast, local-first agentic coder. \\
Be concise and direct. \\
You have tools: run_command, read_file, write_file, list_dir, use_skill, web_fetch. \\
Use them to actually complete tasks rather than describing what to do.\";
\`\`\`

## Related

- #135 — use_skill tool added
- #139 — web_fetch tool added
- See also issue #2 (DEFAULT_SOUL / build_system_prompt_with_soul divergence)" \
  --label "bug"
```

---

## Issue 2 — `DEFAULT_SOUL` and `build_system_prompt_with_soul` have diverged: two sources of truth

**Files:**
- `newt-core/src/memory.rs` — `DEFAULT_SOUL` const (missing `use_skill`, `web_fetch`)
- `newt-tui/src/lib.rs` — `build_system_prompt_with_soul` fallback string (has `use_skill`, `web_fetch`)

### Problem

There are two nearly identical identity strings in the codebase:

1. `DEFAULT_SOUL` in `newt-core` — stale, 4 tools
2. The fallback inside `build_system_prompt_with_soul` in `newt-tui` — current, 6 tools

```rust
// newt-tui/src/lib.rs — build_system_prompt_with_soul
let identity = soul.unwrap_or(
    "You are newt, a small, fast, local-first agentic coder. \
     Be concise and direct. \
     You have tools: run_command, read_file, write_file, list_dir, use_skill, web_fetch. \
     Use them to actually complete tasks rather than describing what to do.",
);
```

This means:
- When a soul file **is** loaded: `SoulProvider::system_prompt_block()` returns
  `DEFAULT_SOUL` (4 tools). `build_system_prompt_with_soul` receives it as
  `soul_text` and uses it verbatim → stale identity.
- When no soul file is found: `build_system_prompt_with_soul` uses its own
  6-tool fallback → current identity.

The two paths produce different results, and fixing one without the other
doesn't solve the underlying two-source-of-truth problem.

### Fix

1. Fix `DEFAULT_SOUL` in `newt-core` (see Issue 1 above).
2. Export `DEFAULT_SOUL` as `pub(crate)` or move it to a shared location.
3. Have `build_system_prompt_with_soul`'s fallback refer to the same constant
   rather than repeating the string — preventing future drift.

### gh command

```bash
gh issue create \
  --title "refactor(soul): DEFAULT_SOUL and build_system_prompt_with_soul fallback have diverged — one source of truth" \
  --body "## Problem

Two nearly-identical identity strings exist in the codebase and have drifted:

| Location | Tools listed |
|---|---|
| \`newt-core/src/memory.rs\` — \`DEFAULT_SOUL\` | 4 (missing \`use_skill\`, \`web_fetch\`) |
| \`newt-tui/src/lib.rs\` — \`build_system_prompt_with_soul\` fallback | 6 (current) |

This means the model identity the agent actually uses depends on an implicit code path:
- Soul file loaded → \`SoulProvider\` returns \`DEFAULT_SOUL\` (stale, 4 tools).
- No soul file → TUI fallback string is used (current, 6 tools).

Every tool addition requires updating TWO places, and the last two (#135, #139) only updated one of them.

## Fix

1. Fix \`DEFAULT_SOUL\` in \`newt-core\` to list all six tools (see related issue).
2. Either export \`DEFAULT_SOUL\` as a shared constant or move it to a single canonical location.
3. Have \`build_system_prompt_with_soul\`'s fallback reference the same constant, not a copy.

## Related

- #135, #139 — the additions that caused the drift
- Issue #1 in this audit (stale DEFAULT_SOUL)" \
  --label "refactor"
```

---

## Issue 3 — `WORKSPACE_DEV_EXEC` allowlist missing common dev tools: `gh`, `python`, `npm`, `node`, `jq`, `make`

**File:** `newt-core/src/config.rs`, line ~290

```rust
const WORKSPACE_DEV_EXEC: &'static [&'static str] = &[
    "cargo", "just", "git", "grep", "rg", "ripgrep", "fd", "find",
    "cat", "ls", "echo", "pwd", "true", "false", "head", "tail",
    "wc", "sort", "uniq", "diff", "patch", "rustfmt", "clippy-driver", "rustup",
];
```

### Problem

`WorkspaceDev` is the default permission preset for every interactive TUI
session. The allowlist covers the Rust toolchain well but is missing tools
that developers routinely reach for in a polyglot workspace:

| Tool | Common use |
|---|---|
| `gh` | File/view GitHub issues, PRs, clone repos |
| `python` / `python3` | Run scripts, quick checks |
| `pip` | Install Python deps |
| `npm` / `node` | JS/TS toolchains |
| `make` | `Makefile`-based projects |
| `jq` | Parse JSON from other tool output |
| `curl` | Quick HTTP checks (when net axis permits) |
| `awk` / `sed` / `cut` | Standard text processing |
| `xargs` | Compose tool chains |
| `which` / `command` | Tooling introspection |
| `env` | Environment inspection |

Any of these silently denies at the `run_command` gate with a
`capability denied` response, with no helpful guidance about how to add
them via `extra_exec`.

### Fix

Expand `WORKSPACE_DEV_EXEC` to include the above. This is not a security
regression — `WorkspaceDev` already grants workspace write and the full Rust
toolchain; these additions are in the same risk tier.

Separately, when `run_command` is denied the error message should mention
`[tui.permissions] extra_exec = ["<tool>"]` as the escape hatch.

### gh command

```bash
gh issue create \
  --title "feat(permissions): expand WORKSPACE_DEV_EXEC allowlist — add gh, python, npm, make, jq, etc." \
  --body "## Problem

\`WorkspaceDev\` is the default permission preset. Its exec allowlist covers the Rust toolchain well but silently denies tools developers routinely need:

\`\`\`
gh, python, python3, pip, npm, node, make, jq, curl, awk, sed, cut, xargs, which, env
\`\`\`

Any of these produces a \`capability denied\` response with no guidance on the escape hatch.

Discovered during a smoke test: \`gh\` is authenticated in the user's external environment but denied inside the agent shell because it isn't in \`WORKSPACE_DEV_EXEC\`.

## Fix

1. Expand \`WORKSPACE_DEV_EXEC\` in \`newt-core/src/config.rs\` to include the tools listed above.
2. When \`run_command\` is denied, print the \`extra_exec\` escape hatch as part of the denial message:
   \`capability denied: 'gh' is not in the exec allowlist — add it via [tui.permissions] extra_exec = [\"gh\"] in your newt config\`

## Risk

Not a security regression — \`WorkspaceDev\` already grants workspace writes and the full Rust toolchain. These additions are in the same risk tier as \`cargo\` and \`git\`.

## Related

- #142 — PATH bootstrap for Rust/Cargo toolchain (same session)" \
  --label "enhancement"
```

---

## Issue 4 — `Config::resolve()` is called multiple times per turn with no caching

**File:** `newt-tui/src/lib.rs`

### Problem

`Config::resolve()` reads and TOML-parses `~/.newt/config.toml` from disk on
**every call**. Within a single user turn the following call sites each
independently invoke `Config::resolve()`:

| Call site | What it reads |
|---|---|
| `resolve_backend_choice()` | `cfg.backends` |
| `resolve_tui()` | `cfg.tui` |
| `max_tool_rounds()` | `cfg.tui.max_tool_rounds` |
| `tool_output_lines()` | `cfg.tui.tool_output_lines` |
| `RollingWindow::from_config()` | `cfg.memory.window` |
| `TokenBudget::from_config()` | `cfg.memory.context_tokens` |
| `SoulProvider::from_config()` | `cfg.memory.soul_file` |

That's at minimum **4 file reads + 4 TOML parses per turn** under the default
config, climbing higher for multi-tool-round turns (e.g. `resolve_tui()` and
`resolve_backend_choice()` are called again after every slash command).

On SSD this is probably sub-millisecond, but it is conceptually wrong (config
could change mid-read), and the pattern will worsen as more call sites are
added.

### Fix

Resolve the config **once** at session start and pass it through — or store it
in a session-scoped struct. The slash-command re-read path (`choice =
resolve_backend_choice()` etc. after each `/command`) is the one intentional
exception and should be called explicitly, not implicitly via cold
`Config::resolve()` calls scattered through helpers.

A minimal fix: make all the `from_config()` / standalone helper functions
accept a `&Config` parameter, and resolve once in `run_chat`. The cold
`Config::resolve()` entry point stays for callers that genuinely need a fresh
read (e.g. `run_init`).

### gh command

```bash
gh issue create \
  --title "perf: Config::resolve() called multiple times per turn — no session-level caching" \
  --body "## Problem

\`Config::resolve()\` reads and TOML-parses \`~/.newt/config.toml\` from disk on every call. Within a single user turn there are at least 4 independent call sites in \`newt-tui/src/lib.rs\`:

- \`resolve_backend_choice()\`
- \`resolve_tui()\`
- \`max_tool_rounds()\`
- \`tool_output_lines()\`

Plus 3 more during session init (\`RollingWindow::from_config()\`, \`TokenBudget::from_config()\`, \`SoulProvider::from_config()\`). In a multi-tool-round turn several of these fire on every round.

On SSD the latency is small but the pattern is semantically wrong (config could change between reads mid-turn) and will grow worse as more call sites are added.

## Fix

Resolve \`Config\` once at session start and pass it as a parameter (or store in a session-scoped struct). The intentional re-read after slash commands should be an explicit call, not an implicit consequence of scattered \`Config::resolve()\` calls in helpers.

Minimal approach:
- \`resolve_backend_choice\`, \`resolve_tui\`, \`max_tool_rounds\`, \`tool_output_lines\` → accept \`&Config\`
- Resolve once at the top of \`run_chat\`, re-resolve explicitly after each slash command
- \`from_config()\` constructors on memory providers stay for out-of-TUI callers but aren't used inside \`run_chat\`

## Impact

- Eliminates 4+ redundant file reads per turn
- Makes turn latency deterministic (no stochastic I/O in the hot path)
- Prevents subtle config-consistency bugs if the file is modified mid-turn" \
  --label "performance"
```

---

*Generated by newt v0.6.6 audit session — branch `issues/code-audit-findings`*
