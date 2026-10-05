# Checked Rust extraction with write_file

`write_file` can move named free functions into a new child module without
retyping their bodies. This requires the `ast` feature (enabled by the default
CLI build):

```json
{
  "path": "src/agentic/helpers.rs",
  "content": "",
  "move_from": {
    "path": "src/agentic/mod.rs",
    "items": ["helper", "other_helper"]
  }
}
```

For the library root or `mod.rs`, the child is in the same directory. For
`foo.rs`, the child is `foo/helpers.rs`. The destination must not exist.
`copy_from` and nonempty `content` cannot be combined with `move_from`.

The tool parses the full source locally with the existing Rust grammar. It
copies the selected items, attached comments, docs and supported attributes;
private functions gain only `pub(super)`. Public visibility stays unchanged.
The source must be reachable through unconditional, ordinary out-of-line
library module declarations; disconnected, binary-only, attributed and
ambiguous module paths are refused. The parent gets a module declaration and matching reexports; the child gets
`use super::*`. Relative visibility, self/super paths, selected macros,
cfg/procedural/unknown attributes, missing/duplicate names and invalid syntax
are refused. This is intentionally a small extraction subset, not general
Rust name resolution or a semantic equivalence proof.

Both files must be readable and their parent directories writable under the
session's grants (complete-file staging needs sibling temporary paths). Checks
run in the owning package, using the same build-root selector as build-bearing
shell commands (#2723). Absolute source and destination paths can name a sibling
worktree already authorized by `--write <worktree>`; relative paths still resolve
against the session workspace. Both files must remain inside the selected build
root. An unapproved sibling is refused with the grant-or-launch-there retry.
The existing Build permission gate must approve any additional build authority;
a write grant alone never authorizes execution.

A confined `cargo check -p <package> --lib` must exit zero before editing and
again afterward. Default Cargo features/configuration apply; other feature
sets, tests and targets need their own checks. A check refusal, failure,
timeout, missing exit code or absence of a kernel sandbox is not success.
Diagnostics are bounded. The check runner is injected in the unit tier.

Each replacement or removal first renames the actual entry into a reserved
recovery name, compares those captured bytes, and publishes using a link that
fails if the original name is occupied. Linux and macOS use the existing held
directory capabilities for this sequence. Other platforms use the same
capture/compare/non-overwriting-link protocol with checked parent paths.

A mismatch returns `CONFLICT` and attempts to put the captured entry back
without overwriting a new occupant. If another writer fills the original name,
both versions survive and the diagnostic identifies the retained recovery path.
Rollback never reports "original files restored" after a conflict.

Displaced entries remain as `.newt-move-*.saved` beside changed files, including
after success and successful rollback. This retention is deliberate: an editor
can still write through an open handle to a captured inode after comparison.
These are live recovery entries, not immutable snapshots. Inspect them and remove
them manually once the extraction and any concurrent editor work are reconciled;
the tool does not automatically discard them. Ordinary staging `.tmp` files are
cleaned up. These recovery locators reuse the existing atomic-file temporary-name
facility; no new undo ledger or content-identity format is introduced.

There can be a short interval with the original name absent between capture and
publication. The operation does not lock arbitrary writers out or provide a
crash-atomic two-file transaction. Success still verifies both source postimages.
Cargo's other effects (build outputs/lockfiles) and newly created empty module
directories remain outside the two source-file inverses.

## Validation

The ordinary unit tier uses an in-memory file store and injected checker for
successful moves, compiler/unavailable failures, stale preimages/postimages
and unwind restoration. AST tests check exact item/docs/attribute bytes and
conservative refusal. The dispatcher regression checks that this mode cannot
fall through to an ordinary write.

The additional CI-only real-resource test grounds those mocks in actual
bounded file publication and real confined Cargo checks. It compiles a tiny,
dependency-free library in an authorized sibling worktree (with an invalid
session manifest proving the check runs in the sibling), then induces a real compiler failure
and verifies byte-for-byte restoration. Run it explicitly on a native host
with Rust and a working kernel build sandbox. The `test` job in
`.github/workflows/ci.yml` also runs this exact ignored test on every PR, after
building the network-guard helper; `.githooks/pre-push` names it as CI-only:

On Linux, first build the existing network-guard helper beside the test binary;
the installed CLI supplies its own equivalent self-exec guard in production.

```sh
env RUSTC_WRAPPER= CARGO_BUILD_JOBS=4 nice -n 10 ionice -c3 \
  cargo build -p newt-core --features ast --bin newt-net-guard
env RUSTC_WRAPPER= CARGO_BUILD_JOBS=4 nice -n 10 ionice -c3 \
  cargo test -p newt-core --features ast --lib \
  agentic::tools::move_from::tests::real_move_from_fixture_compiles_and_failed_check_restores \
  -- --ignored --exact
```

The fully qualified test name is
`agentic::tools::move_from::tests::real_move_from_fixture_compiles_and_failed_check_restores`.
It deliberately fails if the kernel sandbox is unavailable; there is no
skip followed by a success marker. `nice`/`ionice` in the example are Linux
resource controls; use the equivalent native runner policy on other hosts.
