# smart-ab-8: the oracle-validated subset

`smart-ab-12.json` (seed 2260) was run with `--agent oracle` on 2026-09-11
(`/var/tmp/tbench-harbor/oracle-smart-ab-12`, harbor 0.20.0): **8/12 resolved**.
Per the P0a rule, a task the reference solution cannot pass here measures the
apparatus, not the harness, so `smart-ab-8.json` pins the eight that passed.
Excluded, with the verifier's own reason:

| task | verifier |
|---|---|
| `build-cython-ext` | 10/11 passed; `test_pyknotid_repository_tests` fails — an upstream repo's own suite |
| `build-pov-ray` | `FileNotFoundError` on the povray binary; reference build produced no artifact here |
| `make-doom-for-mips` | `frame.bmp` never produced; reference build/render did not complete |
| `portfolio-optimization` | 5/6 passed; `test_performance_and_scalability[8000]` fails at 98 s — machine-speed sensitive |

Backfill to 12 requires oracle-validating four replacements first.
