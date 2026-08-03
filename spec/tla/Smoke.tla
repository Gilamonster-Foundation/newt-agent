---------------------------- MODULE Smoke ----------------------------
\* Toolchain SMOKE test — NOT a model of newt. Its only job is to prove the
\* pinned TLA+ harness (`spec/tla/check.sh` + `tla2tools`) actually parses and
\* model-checks a spec end-to-end in CI, so that when the real models land
\* (`AgentTurn.tla`, `ContextRecovery.tla`; epic #1529 step 6) a green harness is
\* already in place. A committed CHECKED smoke ≠ a premature unchecked model.
EXTENDS Naturals

VARIABLE x

Init == x = 0
Next == x' = (x + 1) % 3
Spec == Init /\ [][Next]_x

\* Safety invariant TLC verifies over the (finite) reachable state space.
Inv == x \in 0..2
=====================================================================
