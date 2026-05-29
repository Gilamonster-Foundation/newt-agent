# cases-deferred — eval cases that need framework work first

Cases in this directory are valid `case.toml` + `workspace/` fixtures
that the current `newt-eval` runner **cannot drive** because they
require capabilities the runner doesn't have yet. They live here
(rather than under `cases/`) so:

1. The bundled-cases test in `src/cases.rs` still passes — that test
   asserts every case under `cases/` has non-empty evaluators and uses
   known evaluator names, and is the gate that catches a malformed
   case getting into the live suite.
2. `newt-eval run` doesn't try to execute them in live mode (it
   defaults to `cases/`).
3. When the framework grows the missing capability, the case can be
   moved back to `cases/` verbatim and the existing evaluators apply
   unchanged.

## Current contents

- **006-cross-host-rename** — exercises `newt mesh ask <peer> "..."`
  (cross-host newt-mesh dispatch). The runner today only knows how to
  drive `newt worker` over ACP stdio; teaching it to spawn a
  `newt mesh announce` responder + read its fingerprint + invoke
  `newt mesh ask` is out of scope for Phase 4 (newt-mesh integration).
  See `docs/decisions/mesh_integration.md`.

  The in-process roundtrip at
  `newt-mesh/tests/inference_roundtrip.rs` covers the same plumbing
  end-to-end in mock form, so we are not blind on the happy path.
