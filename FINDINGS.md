# Probe: can an agent in a harness use `watchdiff-tui` to show the diff it just made?

**Verdict: take the pattern, not the crate.** The headless diff-generation idea
works and is worth having in newt; `watchdiff-tui` itself is the wrong
dependency to carry it.

Everything below is measured, not believed. Probe = [`diffprobe/`](diffprobe/)
(a standalone `[workspace]`, so it never joins the newt build/coverage gate).

## What was run

```
$ cargo run -q -- HEAD~1 text
== probe-marker.txt (added) +3 -0 [Myers]
--- probe-marker.txt
+++ probe-marker.txt
@@ -1,0 +1,3 @@
+# probe marker
+line two
+line three
```

Real diff, real engine (`watchdiff_tui::diff::DiffGenerator`, Myers), driven
from git refs — generated once and emitted two ways: `text` (unified, for a
human scrolling a terminal) and `json` (per-file → per-hunk → per-op, for the
model to re-render as GFM in chat). That half of the concept is **confirmed**.

## What the crate actually is (`watchdiff-tui` 0.2.0, xicv/watchdiff, MIT, ~3.5k LoC)

A **file-watcher TUI**, not a diff-review library:

| module | role |
|---|---|
| `core::watcher` | spawns a thread, keeps `previous_contents: HashMap<PathBuf, String>`, and on every `notify` event diffs old→new (`generate_unified_diff`) into an event log |
| `ui::TuiApp` | `new(watcher: FileWatcher)` → renders **that event log**; browse / fuzzy search / vim nav |
| `diff` | the genuinely reusable part: `DiffGenerator` (Myers + others), `DiffFormatter::format_unified`, `DiffOperation::{Equal,Insert,Delete}` |
| `export` | writes patch files / bundles |

Two claims that reading the source **falsified** (they were in my first draft of
this probe's doc comment, now corrected in [`src/main.rs`](diffprobe/src/main.rs)):

1. *"the human walks hunks accept/reject"* — there is **no accept/reject/apply**
   path. `grep -n "accept\|reject\|apply" src/` returns nothing that writes a
   file; the only `fs::write` calls are in `export`. It is a **monitor**, so it
   cannot be a hunk-level approval gate without writing that half ourselves.
2. *"launch it on the diff I just made"* — you can't hand `TuiApp` a
   `(base, head)` pair. It only ever shows files that changed **while it was
   watching**. Making an agent's already-applied edit appear there means
   *replaying the edit through the filesystem* (write base → let the watcher
   snapshot it → write head), which is racy against its debounce and means the
   agent mutates the user's tree to drive a viewer. Rejected on those grounds
   alone, independent of licensing/deps.

## Why it's the wrong dependency for newt specifically

| concern | evidence |
|---|---|
| duplicate TUI stack | watchdiff pins `ratatui 0.28`; the newt workspace pins `ratatui 0.29` + `crossterm 0.28` (`newt-agent/Cargo.toml:256`) → two ratatui majors in one binary |
| closure weight | pulls `notify 6`, `tokio` **full**, `clap 4`, `ctrlc`, `chrono`, `ignore`, `syntect 5`, `tracing-subscriber` for a tool that already has all the parts it needs |
| newt has no watcher, on purpose | `grep "^notify" newt-agent/Cargo.toml` → nothing. newt knows what changed because **its own tool layer made the change** (`newt-tools/src/patch.rs`, `edit`/`apply_patch` in `pyo3_module.rs`); a filesystem watcher re-derives that fact from the outside, slower and with more ways to be wrong |
| license | watchdiff is `MIT`; the workspace is Apache-2.0 (`license.workspace`) — compatible, but another dual-license line to track for a crate at 0.2.0 with one maintainer |
| the useful half is ~nothing | its diff engine is a wrapper over `similar 2.6`; newt already depends on `diffy 0.4` (`newt-agent/Cargo.toml:196`), which is itself built on `similar` — the diff engine is not a gap |

## What to build instead (the pattern worth keeping)

Generate **one** diff, render it **twice** — on primitives newt already owns:

1. `diff::Report` (per-file → per-hunk → per-op, like `diffprobe`'s JSON) handed
   to the model → GFM in chat for the plain scroller.
2. the same report in a newt-native ratatui modal for the operator, if/when
   hunk-level review is wanted — and *that* modal is where accept/reject would
   live, backed by the applier newt already has (`newt-tools/src/patch.rs`:
   `FuzzyApplier` default, `DiffyApplier` behind `NEWT_PATCH_APPLIER`).

Adjacent, already in the workspace and worth reusing for any
"same change, seen twice" identity work: newt already depends on
`content-addressable` **with `unstable-merkle`** (`newt-agent/Cargo.toml:136`,
BLAKE3 + CIDv1 + dag-cbor; org repo
[`../content-addressable`](../content-addressable)) — that, not a watcher, is
the honest way to say "this hunk is the one I already showed you".

## Reproduce

```bash
cd newt-agent-watchdiff-probe/diffprobe
cargo run -- HEAD~1 text     # unified diff
cargo run -- HEAD~1 json     # per-hunk/per-op report
```

Nothing here needs to merge into newt; it is a throwaway probe kept on branch
`newt-agent-watchdiff-probe`.
