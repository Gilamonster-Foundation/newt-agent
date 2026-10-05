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
run in the owning package inside the current session workspace. The existing
Build permission gate must approve any additional build authority. Sibling
worktrees remain subject to the separate containment work in #2723; use a
session rooted in the intended worktree until that is available.

A confined `cargo check -p <package> --lib` must exit zero before editing and
again afterward. Default Cargo features/configuration apply; other feature
sets, tests and targets need their own checks. A check refusal, failure,
timeout, missing exit code or absence of a kernel sandbox is not success.
Diagnostics are bounded. The check runner is injected in the unit tier.

Each publication checks its preimage; success also verifies both postimages.
Failure restores the original source and removes the tool-created child only
while their bytes still match the tool's output. An intervening edit is
preserved and reported as incomplete rollback. These are optimistic checks,
not a lock against arbitrary external writers or a crash-atomic two-file
transaction. Cargo's other effects (build outputs/lockfiles), and newly created
empty module directories, are outside the two source-file inverses. Linux and
macOS use held directory capabilities; other platforms use the existing
atomic-file helpers with checked parent paths.

## Validation

The ordinary unit tier uses an in-memory file store and injected checker for
successful moves, compiler/unavailable failures, stale preimages/postimages
and unwind restoration. AST tests check exact item/docs/attribute bytes and
conservative refusal. The dispatcher regression checks that this mode cannot
fall through to an ordinary write.

The additional CI-only real-resource test grounds those mocks in actual
bounded file publication and real confined Cargo checks. It compiles a tiny,
dependency-free library after extraction, then induces a real compiler failure
and verifies byte-for-byte restoration. Run it explicitly on a native host
with Rust and a working kernel build sandbox:

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
