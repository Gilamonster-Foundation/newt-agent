# Authoring newt-eval cases

A case lives at `cases/NNN-name/` and is two things:

- `case.toml` — metadata, the prompt, the evaluators, and a golden
  `mock_response` diff.
- `workspace/` — the initial files, copied into a tempdir for each run.

## The emission contract (read this first)

**The system prompt owns the emission shape. Case prompts describe the
task — never the format.**

`newt-coder`'s `WHOLE_FILE_SYSTEM_PROMPT` already tells the model how to
respond. If a case prompt *also* says "respond with a unified diff only"
(or "emit the whole file only", "respond with a JSON object only", …) it
**contradicts** the system prompt. Different models resolve that conflict
differently, so the bake-off ends up measuring prompt-resolution
preference instead of coding capability (this is the bug #31 fixed —
stripping the line flipped model rankings).

A lint enforces this: `cargo test -p newt-eval --test case_prompt_lint`
fails if any prompt matches

```
(?i)(unified diff|whole file|complete (updated )?file|json object|fenced code block|patch only) … only
```

Write the task. Let the system prompt own the shape.

- Bad:  `Rename greet to hello. Respond with a unified diff only.`
- Good: `Rename greet to hello, updating the call site so tests pass.`

## `case.toml` fields

| Field | Meaning |
|-------|---------|
| `name` | Matches the directory name. |
| `description` | One line. |
| `language` | `"rust"` today; language-specific evaluators key off this. |
| `difficulty` | `"L1"` (saturated single edits), `"L2"` (multi-step single-domain), `"L3"` (cross-domain). Defaults to `L1`. Filter with `newt-eval run --difficulty L2`. |
| `prompt` | The task. **No emission-shape directive** (see above). |
| `evaluators` | Usually all five: `diff_nonempty`, `diff_applies`, `rust_compiles`, `tests_pass`, `pattern_match`. |
| `expected_patterns` | ≥1 regex; at least one must match the captured diff. Prefer plain substrings (avoid regex metacharacters unless you mean them). |
| `[mock_response].content` | The golden diff returned verbatim by the mock Ollama. |

## Authoring the golden diff (don't hand-count hunks)

`diff_applies` runs real `git apply --check` against the model's raw
emission, so the golden must be a byte-accurate unified diff — blank
context lines need their leading space, hunk headers need correct counts,
and new files need a `--- /dev/null` section. Hand-authoring these is how
007/008 ended up with corrupt headers the lenient parser hid.

Generate goldens from a real `git diff` instead:

1. Write `workspace/` (the seed) and a throwaway target tree.
2. `git init && git add -A && git commit -m base`
3. Apply your intended change to the target.
4. `git add -A && git diff --cached` — `--cached` so **new** files
   (e.g. `src/util.rs`) are included.
5. Strip the git-only header lines (`diff --git`, `index`, `new file
   mode`, …); keep the `---`/`+++`/`@@` sections. The in-house fuzzy
   applier otherwise absorbs those lines as phantom context.

Then verify the whole case end-to-end:

```bash
cargo test -p newt-eval --test mock_e2e
```

Every case must go GREEN on all five evaluators in mock mode before it
lands.
